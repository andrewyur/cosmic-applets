use bluer::agent::{Agent, ReqError, RequestConfirmation};
use futures::FutureExt;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum AgentEvent {
    RequestConfirmation(u32, bluer::Address, oneshot::Sender<bool>),
}

/// Bluetooth authorization agent (handles generating/displaying pin codes and passkeys)
pub fn create_agent(output: mpsc::UnboundedSender<AgentEvent>) -> Agent {
    Agent {
        request_default: false,
        request_confirmation: Some({
            let output = output.clone();
            Box::new(move |req| {
                let output = output.clone();
                request_confirmation(req, output).boxed()
            })
        }),
        ..Default::default()
    }
}

/// Both devices show same code, user confirms that they match
async fn request_confirmation(req: RequestConfirmation, output: mpsc::UnboundedSender<AgentEvent>) -> Result<(), ReqError> {
    tracing::info!("agent received confirmation request...");

    let (tx, rx) = oneshot::channel();

    _ = output.send(AgentEvent::RequestConfirmation(req.passkey, req.device, tx));

    match rx.await {
        Ok(true) => Ok(()),
        _ => Err(ReqError::Rejected)
    }
}