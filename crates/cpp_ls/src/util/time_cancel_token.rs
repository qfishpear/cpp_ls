use std::time::Duration;

use tokio_util::sync::CancellationToken;

pub fn time_cancel_token(time: Duration) -> CancellationToken {
    let cancel_token = CancellationToken::new();
    let cancel_handle = cancel_token.clone();
    crate::rt::spawn(async move {
        crate::rt::sleep(time).await;
        cancel_handle.cancel();
    });
    cancel_token
}
