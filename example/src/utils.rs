use std::future::IntoFuture;
use std::time::Duration;

use futures::future::{TryFuture, TryFutureExt};
use indicatif::ProgressBar;

pub fn spinner() -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner
}

pub async fn try_timeout<F>(
    duration: Duration,
    future: F,
) -> Result<<F::IntoFuture as TryFuture>::Ok, Box<dyn std::error::Error>>
where
    F: IntoFuture,
    F::IntoFuture: TryFuture,
    Box<dyn std::error::Error>: From<<F::IntoFuture as TryFuture>::Error>,
{
    Ok(
        match tokio::time::timeout(
            duration,
            TryFutureExt::into_future(IntoFuture::into_future(future)),
        )
        .await
        {
            Ok(res) => res?,
            Err(elapsed) => return Err(format!("Timeout after {elapsed:?}").into()),
        },
    )
}
