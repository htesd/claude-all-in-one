//! 最小 protobuf 编解码(wire format),不引入 prost。
//!
//! 覆盖 `agent.v1.AgentService/Run` 往返所需的全部线格式:varint(wt=0)、
//! length-delimited(wt=2:string / bytes / 嵌套 message)、fixed64(wt=1)与
//! fixed32(wt=5)。
//!
//! ⚠️ fixed64 **必须带出字节**,不能只跳过:工具调用的参数值是
//! `google.protobuf.Value`,数字装在 `number_value`(field 2,double = fixed64)。
//! 早先这里只记录"有个 fixed64"而丢掉内容,后果是模型传数字参数时我们解出空串
//! (实测:opencode 的 `read` 收到 `limit=""` / `offset=""`,工具失败,模型无限重试)。
//!
//! 逐字对齐官方客户端的字段号来自解包 workbench bundle 的 `makeMessageType`
//! schema(3.14.27),见 crate 顶层文档。

/// protobuf 写入器:按字段号顺序追加,产出线格式字节。
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn write_varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                self.buf.push(b | 0x80);
            } else {
                self.buf.push(b);
                break;
            }
        }
    }

    fn write_tag(&mut self, field: u32, wire_type: u32) {
        self.write_varint(((field as u64) << 3) | wire_type as u64);
    }

    /// 写一个 string 字段(wt=2)。
    pub fn string(&mut self, field: u32, s: &str) {
        self.bytes(field, s.as_bytes());
    }

    /// 写一个 length-delimited 字段(wt=2):bytes / 嵌套 message。
    pub fn bytes(&mut self, field: u32, b: &[u8]) {
        self.write_tag(field, 2);
        self.write_varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    /// 写一个嵌套 message 字段(wt=2):传入已序列化的子 message 字节。
    pub fn message(&mut self, field: u32, msg: &Writer) {
        self.bytes(field, &msg.buf);
    }

    /// 写一个 bool 字段(wt=0)。Cursor 侧 proto3 optional bool,false 通常可省;
    /// 这里显式写出以便需要时强制发送。
    #[allow(dead_code)] // 保留:wire format 的完整一档,回传工具结果时会用到
    pub fn bool(&mut self, field: u32, v: bool) {
        self.write_tag(field, 0);
        self.write_varint(if v { 1 } else { 0 });
    }

    /// 写一个 varint 数值字段(wt=0):int32 / enum / uint32 等。
    pub fn uint(&mut self, field: u32, v: u64) {
        self.write_tag(field, 0);
        self.write_varint(v);
    }

    /// 写一个 double 字段(wt=1)。`google.protobuf.Value.number_value` 用它。
    #[allow(dead_code)] // 目前只有测试造帧用;回传工具结果(请求侧 field 2)时会用到
    pub fn double(&mut self, field: u32, v: f64) {
        self.write_tag(field, 1);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// 解码出的一个字段值(借用底层缓冲)。
#[derive(Debug)]
pub enum Value<'a> {
    Varint(u64),
    Len(&'a [u8]),
    /// 小端 4 字节原值(float / fixed32 / sfixed32)。
    Fixed32([u8; 4]),
    /// 小端 8 字节原值(double / fixed64 / sfixed64)。
    Fixed64([u8; 8]),
}

impl Value<'_> {
    /// 按 `double` 读 fixed64。非 fixed64 → `None`。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Fixed64(b) => Some(f64::from_le_bytes(*b)),
            Value::Fixed32(b) => Some(f32::from_le_bytes(*b) as f64),
            _ => None,
        }
    }
}

/// 流式字段读取器:逐字段吐 `(field_no, Value)`。遇畸形/截断返回 `None` 结束。
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = (u32, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let key = self.read_varint()?;
        let field = (key >> 3) as u32;
        let wire_type = (key & 7) as u32;
        match wire_type {
            0 => {
                let v = self.read_varint()?;
                Some((field, Value::Varint(v)))
            }
            2 => {
                let len = self.read_varint()? as usize;
                let end = self.pos.checked_add(len)?;
                if end > self.buf.len() {
                    return None;
                }
                let slice = &self.buf[self.pos..end];
                self.pos = end;
                Some((field, Value::Len(slice)))
            }
            5 => {
                let end = self.pos.checked_add(4)?;
                if end > self.buf.len() {
                    return None;
                }
                let mut b = [0u8; 4];
                b.copy_from_slice(&self.buf[self.pos..end]);
                self.pos = end;
                Some((field, Value::Fixed32(b)))
            }
            1 => {
                let end = self.pos.checked_add(8)?;
                if end > self.buf.len() {
                    return None;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.buf[self.pos..end]);
                self.pos = end;
                Some((field, Value::Fixed64(b)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_small_and_large() {
        let mut w = Writer::new();
        w.uint(1, 1);
        w.uint(2, 300);
        w.uint(3, 0);
        let bytes = w.into_bytes();
        let mut got = Vec::new();
        for (f, v) in Reader::new(&bytes) {
            if let Value::Varint(n) = v {
                got.push((f, n));
            }
        }
        assert_eq!(got, vec![(1, 1), (2, 300), (3, 0)]);
    }

    #[test]
    fn string_and_nested_message() {
        let mut inner = Writer::new();
        inner.string(1, "hello");
        inner.uint(2, 1);
        let mut outer = Writer::new();
        outer.message(1, &inner);
        let bytes = outer.into_bytes();

        // 外层 field 1 = 嵌套 message
        let mut r = Reader::new(&bytes);
        let (f, v) = r.next().unwrap();
        assert_eq!(f, 1);
        let sub = match v {
            Value::Len(s) => s,
            _ => panic!("expected len-delimited"),
        };
        // 解嵌套
        let mut text = None;
        let mut ty = None;
        for (ff, vv) in Reader::new(sub) {
            match (ff, vv) {
                (1, Value::Len(s)) => text = Some(String::from_utf8_lossy(s).to_string()),
                (2, Value::Varint(n)) => ty = Some(n),
                _ => {}
            }
        }
        assert_eq!(text.as_deref(), Some("hello"));
        assert_eq!(ty, Some(1));
    }

    #[test]
    fn fixed64_carries_its_bytes_out() {
        // 手写字节,不用 Writer —— 这条是要钉住 wire format 本身:
        // field 2, wire type 1 → tag = (2<<3)|1 = 0x11,后跟 8 字节小端 double。
        let mut bytes = vec![0x11u8];
        bytes.extend_from_slice(&200.0f64.to_le_bytes());
        let got: Vec<_> = Reader::new(&bytes).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 2);
        assert_eq!(got[0].1.as_f64(), Some(200.0), "丢了 fixed64 的字节 = 数值参数解成空");
    }

    #[test]
    fn double_roundtrips_through_writer() {
        let mut w = Writer::new();
        w.double(2, -1.5);
        let b = w.into_bytes();
        assert_eq!(b[0], 0x11, "tag 必须是 field 2 / wt 1");
        assert_eq!(
            Reader::new(&b).next().unwrap().1.as_f64(),
            Some(-1.5)
        );
    }

    #[test]
    fn truncated_input_stops_cleanly() {
        // field 1 声明 len=10 但只给 3 字节 → 迭代器应结束而非 panic。
        let bytes = [0x0a, 0x0a, b'a', b'b', b'c'];
        let collected: Vec<_> = Reader::new(&bytes).collect();
        assert!(collected.is_empty());
    }
}
