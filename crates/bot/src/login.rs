use crate::app::{App, TgBot};
use crate::ui::{self, UiCallback};
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthSession, Credential,
};
use feedbot_core::{CredentialRepo, Scope};
use std::sync::Arc;
use std::time::Duration;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::payloads::{SendMessageSetters, SendPhotoSetters};
use teloxide::prelude::*;
use teloxide::types::{ChatKind, InlineKeyboardButton, InlineKeyboardMarkup, InputFile};
use tokio_util::sync::CancellationToken;

pub type LoginDialogue = Dialogue<LoginState, InMemStorage<LoginState>>;

const QR_POLL_MAX: u32 = 65;

#[derive(Clone, Default, Debug)]
pub enum LoginState {
    #[default]
    Idle,
    InProgress {
        extractor_id: String,
        session: AuthSession,
        secret: bool,
        global: bool,
        cancel: CancellationToken,
    },
}

pub async fn begin(
    bot: &TgBot,
    msg: &Message,
    app: Arc<App>,
    dialogue: LoginDialogue,
    name: String,
) -> anyhow::Result<()> {
    if !matches!(msg.chat.kind, ChatKind::Private(_)) {
        bot.send_message(msg.chat.id, "请私聊使用 /login").await?;
        return Ok(());
    }
    if name.trim().is_empty() {
        return Ok(());
    }
    let Some(ext) = app.registry.resolve_name(&name) else {
        bot.send_message(
            msg.chat.id,
            format!(
                "未知平台 <code>{}</code>。用 /help 查看列表。",
                teloxide::utils::html::escape(name.trim())
            ),
        )
        .await?;
        return Ok(());
    };
    let Some(auth) = ext.auth() else {
        bot.send_message(msg.chat.id, "此平台不需要登录").await?;
        return Ok(());
    };
    let method = auth.methods().first().copied();
    let Some(method) = method else {
        bot.send_message(msg.chat.id, "此平台没有可用的登录方式")
            .await?;
        return Ok(());
    };
    let global = msg.text().is_some_and(|t| t.contains("global"))
        && app.is_admin(msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0));
    if msg.text().is_some_and(|t| t.contains("global")) && !global {
        bot.send_message(msg.chat.id, "只有管理员能写入全局凭据")
            .await?;
        return Ok(());
    }
    let mut session = AuthSession {
        extractor_id: ext.id().into(),
        method: Some(method),
        ..AuthSession::default()
    };
    let challenge = auth.begin(method, &mut session).await?;
    let secret = matches!(challenge, AuthChallenge::AskText { secret: true, .. });
    let cancel = CancellationToken::new();
    let poll_interval = match &challenge {
        AuthChallenge::ShowQr { poll_interval, .. } => Some(*poll_interval),
        _ => None,
    };
    render_challenge(bot, msg.chat.id, &challenge).await?;
    dialogue
        .update(LoginState::InProgress {
            extractor_id: ext.id().into(),
            session: session.clone(),
            secret,
            global,
            cancel: cancel.clone(),
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(interval) = poll_interval {
        let bot = bot.clone();
        let chat = msg.chat.id;
        let app = app.clone();
        let dialogue = dialogue.clone();
        let extractor_id = ext.id().to_string();
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
        tokio::spawn(async move {
            poll_qr(
                bot,
                chat,
                app,
                dialogue,
                extractor_id,
                session,
                global,
                user_id,
                cancel,
                interval,
            )
            .await;
        });
    }
    Ok(())
}

/// Clear an in-progress login. Returns whether a login was actually running.
pub async fn abort(dialogue: &LoginDialogue) -> bool {
    let state = dialogue.get().await.ok().flatten();
    if let Some(LoginState::InProgress { cancel, .. }) = state {
        cancel.cancel();
        dialogue.update(LoginState::Idle).await.ok();
        true
    } else {
        false
    }
}

pub fn is_abort_text(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "/cancel" | "cancel" | "取消" | "退出" | "/退出"
    ) || lower.starts_with("/cancel@")
}

pub async fn on_text(
    bot: TgBot,
    msg: Message,
    app: Arc<App>,
    dialogue: LoginDialogue,
) -> anyhow::Result<()> {
    let state = dialogue
        .get()
        .await
        .ok()
        .flatten()
        .unwrap_or(LoginState::Idle);
    let LoginState::InProgress {
        extractor_id,
        mut session,
        secret,
        global,
        cancel,
    } = state
    else {
        return crate::handlers::handle_url_message(bot, msg, app).await;
    };

    let Some(text) = msg.text().or(msg.caption()).map(str::to_string) else {
        bot.send_message(
            msg.chat.id,
            "请按提示完成登录，或点「取消登录」/ 发送「取消」退出。",
        )
        .await?;
        return Ok(());
    };
    if is_abort_text(&text) {
        abort(&dialogue).await;
        bot.send_message(msg.chat.id, "已退出登录。").await?;
        return Ok(());
    }
    if text.trim_start().starts_with('/') {
        abort(&dialogue).await;
        bot.send_message(msg.chat.id, "已退出登录。请重新发送该命令。")
            .await?;
        return Ok(());
    }
    if session.method == Some(AuthMethod::QrCode) {
        bot.send_message(
            msg.chat.id,
            "请扫码登录，或点「取消登录」/ 发送「取消」退出。",
        )
        .await?;
        return Ok(());
    }
    if secret {
        let _ = bot.delete_message(msg.chat.id, msg.id).await;
    }
    let Some(ext) = app.registry.resolve_name(&extractor_id) else {
        cancel.cancel();
        dialogue.update(LoginState::Idle).await.ok();
        return Ok(());
    };
    let Some(auth) = ext.auth() else {
        cancel.cancel();
        dialogue.update(LoginState::Idle).await.ok();
        return Ok(());
    };
    match auth.advance(&mut session, AuthInput::Text(text)).await {
        Ok(AuthProgress::Continue(ch)) => {
            let secret = matches!(ch, AuthChallenge::AskText { secret: true, .. });
            render_challenge(&bot, msg.chat.id, &ch).await?;
            dialogue
                .update(LoginState::InProgress {
                    extractor_id,
                    session,
                    secret,
                    global,
                    cancel,
                })
                .await
                .ok();
        }
        Ok(AuthProgress::Completed(cred)) => {
            let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
            finish_login(
                &bot,
                msg.chat.id,
                &app,
                &dialogue,
                &extractor_id,
                global,
                user_id,
                cred,
                secret,
            )
            .await?;
        }
        Ok(AuthProgress::Pending | AuthProgress::Hint(_)) => {}
        Err(e) => {
            bot.send_message(
                msg.chat.id,
                format!("{e}\n可重新发送 Cookie，或点「取消登录」/ 发送「取消」退出。"),
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn poll_qr(
    bot: TgBot,
    chat: ChatId,
    app: Arc<App>,
    dialogue: LoginDialogue,
    extractor_id: String,
    mut session: AuthSession,
    global: bool,
    user_id: i64,
    cancel: CancellationToken,
    interval: Duration,
) {
    let interval = if interval.is_zero() {
        Duration::from_secs(3)
    } else {
        interval
    };
    for _ in 0..QR_POLL_MAX {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        if cancel.is_cancelled() {
            return;
        }
        let Some(ext) = app.registry.resolve_name(&extractor_id) else {
            break;
        };
        let Some(auth) = ext.auth() else {
            break;
        };
        match auth.advance(&mut session, AuthInput::Poll).await {
            Ok(AuthProgress::Pending) => {}
            Ok(AuthProgress::Hint(text)) => {
                let _ = bot.send_message(chat, text).await;
            }
            Ok(AuthProgress::Continue(AuthChallenge::ShowQr { .. })) => {}
            Ok(AuthProgress::Continue(ch)) => {
                let _ = render_challenge(&bot, chat, &ch).await;
            }
            Ok(AuthProgress::Completed(cred)) => {
                let _ = finish_login(
                    &bot,
                    chat,
                    &app,
                    &dialogue,
                    &extractor_id,
                    global,
                    user_id,
                    cred,
                    false,
                )
                .await;
                return;
            }
            Err(AuthError::SessionExpired) => {
                dialogue.update(LoginState::Idle).await.ok();
                let _ = bot
                    .send_message(chat, "二维码已过期，请重新 /login。")
                    .await;
                return;
            }
            Err(e) => {
                let _ = bot
                    .send_message(
                        chat,
                        format!("{e}\n可继续等待扫码，或点「取消登录」/ 发送「取消」退出。"),
                    )
                    .await;
            }
        }
    }
    if cancel.is_cancelled() {
        return;
    }
    dialogue.update(LoginState::Idle).await.ok();
    let _ = bot
        .send_message(chat, "扫码超时，请重新 /login。")
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn finish_login(
    bot: &TgBot,
    chat: ChatId,
    app: &App,
    dialogue: &LoginDialogue,
    extractor_id: &str,
    global: bool,
    user_id: i64,
    cred: Credential,
    secret: bool,
) -> anyhow::Result<()> {
    let (scope, scope_id) = if global {
        (Scope::Global, 0)
    } else {
        (Scope::User, user_id)
    };
    app.store.save(scope, scope_id, &cred).await?;
    if let Some(LoginState::InProgress { cancel, .. }) = dialogue.get().await.ok().flatten() {
        cancel.cancel();
    }
    dialogue.update(LoginState::Idle).await.ok();
    let extra = if secret {
        "含凭据的消息已删除。"
    } else {
        ""
    };
    let name = app
        .registry
        .resolve_name(extractor_id)
        .map(|e| e.display_name().to_string())
        .unwrap_or_else(|| extractor_id.to_string());
    bot.send_message(chat, format!("已保存 {name} 登录态。{extra}"))
        .await?;
    Ok(())
}

async fn render_challenge(
    bot: &TgBot,
    chat: ChatId,
    challenge: &AuthChallenge,
) -> anyhow::Result<()> {
    let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "取消登录",
        ui::encode(&UiCallback::CancelLogin),
    )]]);
    match challenge {
        AuthChallenge::AskText { prompt, .. } => {
            bot.send_message(
                chat,
                format!("{prompt}\n\n发送「取消」或 /cancel 也可退出。"),
            )
            .reply_markup(kb)
            .await?;
        }
        AuthChallenge::ShowQr { png, .. } => {
            bot.send_photo(chat, InputFile::memory(png.clone()))
                .caption("请扫码登录。发送「取消」或 /cancel 也可退出。")
                .reply_markup(kb)
                .await?;
        }
        AuthChallenge::OpenUrl { url, hint } => {
            bot.send_message(chat, format!("{hint}\n{url}"))
                .reply_markup(kb)
                .await?;
        }
        AuthChallenge::Done => {
            bot.send_message(chat, "登录完成").await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_text_matches_cancel_words() {
        assert!(is_abort_text("取消"));
        assert!(is_abort_text(" 退出 "));
        assert!(is_abort_text("/cancel"));
        assert!(is_abort_text("/Cancel"));
        assert!(is_abort_text("/cancel@MyBot"));
        assert!(is_abort_text("cancel"));
        assert!(!is_abort_text("SID=abc"));
        assert!(!is_abort_text("/login youtube"));
        assert!(!is_abort_text(""));
    }
}
