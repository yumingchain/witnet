//! Defines an actor to control system run and shutdown.
//!
//! See the [`Controller`] struct for more information.
use std::time::Duration;

use actix_web::actix::*;
use failure::Fail;
use futures::{future, Future};

/// Actor to start and gracefully stop an actix system.
///
/// This actor contains a static `run` method which will run an actix system and block the current
/// thread until the system shuts down again.
///
/// To shut down more gracefully, other actors can register with the [`Subscribe`] message. When a
/// shutdown signal is sent to the process, they will receive a [`Shutdown`] message with an
/// optional timeout. They can respond with a future, after which they will be stopped. Once all
/// registered actors have stopped successfully, the entire system will stop.
pub struct Controller {
    /// Configured timeout for graceful shutdown
    timeout: Duration,
    /// Subscribed actors for the shutdown message.
    subscribers: Vec<Recipient<Shutdown>>,
}

impl Default for Controller {
    fn default() -> Self {
        Controller {
            timeout: Duration::from_secs(10),
            subscribers: Vec::new(),
        }
    }
}

impl Controller {
    /// Performs a graceful shutdown with the given timeout.
    ///
    /// This sends a `Shutdown` message to all subscribed actors and
    /// waits for them to finish. As soon as all actors have
    /// completed, `Controller::stop` is called.
    pub fn shutdown(&mut self, ctx: &mut Context<Self>, timeout: Option<Duration>) {
        let futures: Vec<_> = self
            .subscribers
            .iter()
            .map(|addr| {
                addr.send(Shutdown { timeout })
                    .map(|_| ())
                    .map_err(|e| log::error!("Shutdown failed: {}", e))
            })
            .collect();

        future::join_all(futures)
            .into_actor(self)
            .and_then(|_, _, ctx| {
                ctx.stop();
                fut::ok(())
            })
            .spawn(ctx)
    }
}

impl Actor for Controller {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        signal::ProcessSignals::from_registry()
            .do_send(signal::Subscribe(ctx.address().recipient()));
    }
}

impl Supervised for Controller {}

impl SystemService for Controller {}

impl Handler<signal::Signal> for Controller {
    type Result = <signal::Signal as Message>::Result;

    fn handle(&mut self, message: signal::Signal, ctx: &mut Self::Context) -> Self::Result {
        match message.0 {
            signal::SignalType::Quit => {
                log::info!("SIGQUIT received, exiting");
                self.shutdown(ctx, None);
            }
            signal::SignalType::Term | signal::SignalType::Int => {
                let timeout = self.timeout;
                log::info!(
                    "SIGTERM/SIGINT received, stopping with timeout: {}s",
                    timeout.as_secs()
                );
                self.shutdown(ctx, Some(timeout));
            }
            _ => (),
        }
    }
}

/// Subscription message for [`Shutdown`](Shutdown) events
pub struct Subscribe(pub Recipient<Shutdown>);

impl Message for Subscribe {
    type Result = ();
}

impl Handler<Subscribe> for Controller {
    type Result = <Subscribe as Message>::Result;

    fn handle(&mut self, msg: Subscribe, _ctx: &mut Self::Context) -> Self::Result {
        self.subscribers.push(msg.0)
    }
}

/// Shutdown request message sent by the [`Controller`](Controller) to subscribed actors.
///
/// The specified timeout is only a hint to the implementor of this message. A handler has to ensure
/// that it doesn't take significantly longer to resolve the future. Ideally, open work is persisted
/// or finished in an orderly manner but no new requests are accepted anymore.
///
/// The implementor may indicate a timeout by responding with `Err(TimeoutError)`. At the moment,
/// this does not have any consequences for the shutdown.
pub struct Shutdown {
    /// The timeout for this shutdown. `None` indicates an immediate forced shutdown.
    pub timeout: Option<Duration>,
}

/// Result type with error set to [`TimeoutError`](TimeoutError)
pub type ShutdownResult = Result<(), TimeoutError>;

impl Message for Shutdown {
    type Result = ShutdownResult;
}

/// Error to indicate a timeout in a shutdown.
///
/// See [`Shutdown`](Shutdown) for more information.
#[derive(Debug, Fail, Copy, Clone, Eq, PartialEq)]
#[fail(display = "timed out")]
pub struct TimeoutError;
