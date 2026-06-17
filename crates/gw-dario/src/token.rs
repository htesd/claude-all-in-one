use gw_core::account::Account;
use gw_core::error::UpstreamError;
pub(crate) async fn refresh(_c: &reqwest::Client, account: &Account) -> Result<Account, UpstreamError> {
    Ok(account.clone())
}
