use crate::app::TgBot;
use axum::Router;
use axum::routing::get;
use std::net::SocketAddr;
use std::time::Duration;
use teloxide::dispatching::{DefaultKey, Dispatcher};
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::payloads::DeleteWebhookSetters;
use teloxide::requests::Requester;
use teloxide::types::AllowedUpdate;
use teloxide::update_listeners::{Polling, webhooks};
use url::Url;

#[derive(Debug, Clone)]
pub enum Transport {
    Polling {
        timeout: Duration,
        drop_pending: bool,
    },
    Webhook {
        addr: SocketAddr,
        public_url: Url,
        secret_token: String,
        drop_pending: bool,
    },
}

pub async fn dispatch(
    bot: TgBot,
    mut dispatcher: Dispatcher<TgBot, anyhow::Error, DefaultKey>,
    transport: Transport,
) -> Result<(), teloxide::RequestError> {
    match transport {
        Transport::Polling {
            timeout,
            drop_pending,
        } => {
            bot.delete_webhook()
                .drop_pending_updates(drop_pending)
                .await?;
            tracing::info!(drop_pending, "deleted Telegram webhook before polling");
            let mut builder = Polling::builder(bot.clone())
                .timeout(timeout)
                .allowed_updates(vec![AllowedUpdate::Message, AllowedUpdate::CallbackQuery]);
            if drop_pending {
                builder = builder.drop_pending_updates();
            }
            let listener = builder.build();
            dispatcher
                .try_dispatch_with_listener(listener, LoggingErrorHandler::new())
                .await
        }
        Transport::Webhook {
            addr,
            public_url,
            secret_token,
            drop_pending,
        } => {
            let mut options = webhooks::Options::new(addr, public_url);
            if drop_pending {
                options = options.drop_pending_updates();
            }
            if !secret_token.is_empty() {
                options = options.secret_token(secret_token);
            }
            let address = options.address;
            let (listener, stop_flag, router) =
                webhooks::axum_to_router(bot.clone(), options).await?;
            let router = Router::new()
                .route("/healthz", get(|| async { "ok" }))
                .merge(router);
            tokio::spawn(async move {
                match tokio::net::TcpListener::bind(address).await {
                    Ok(tcp) => {
                        if let Err(e) = axum::serve(tcp, router)
                            .with_graceful_shutdown(stop_flag)
                            .await
                        {
                            tracing::error!(error = %e, "webhook server error");
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "failed to bind webhook addr"),
                }
            });
            dispatcher
                .try_dispatch_with_listener(listener, LoggingErrorHandler::new())
                .await
        }
    }
}
