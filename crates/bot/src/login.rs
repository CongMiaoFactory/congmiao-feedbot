use crate::app::{App, TgBot};
use crate::ui::{self, UiCallback};
use feedbot_core::Scope;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthSession, Credential,
};
use std::sync::Arc;
use std::time::Duration;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::payloads::{SendMessageSetters, SendPhotoSetters};
use teloxide::prelude::*;
use teloxide::types::{ChatKind, InlineKeyboardButton, InlineKeyboardMarkup, InputFile};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

pub type LoginDialogue = Dialogue<LoginState, InMemStorage<LoginState>>;

const QR_POLL_MAX: u32 = 65;
/// Consecutive poll errors after which a QR login is abandoned.
const QR_POLL_MAX_ERRORS: u32 = 5;

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

/// `/login` / `/logout` arguments: platform name plus an optional `global`
/// keyword (operators only) that targets the shared credential.
pub fn parse_target(args: &str) -> (String, bool) {
    let mut global = false;
    let mut name = String::new();
    for token in args.split_whitespace() {
        if token.eq_ignore_ascii_case("global") {
            global = true;
        } else if name.is_empty() {
            name = token.to_string();
        }
    }
    (name, global)
}

/// Start a login. Any login already running in this dialogue is aborted first
/// so its QR poller cannot save a credential afterwards.
pub async fn begin(
    bot: &TgBot,
    msg: &Message,
    app: Arc<App>,
    dialogue: LoginDialogue,
    name: String,
    global: bool,
) -> anyhow::Result<()> {
    abort(&dialogue).await;
    if !matches!(msg.chat.kind, ChatKind::Private(_)) {
        bot.send_message(msg.chat.id, "请私聊使用 /login").await?;
        return Ok(());
    }
    if name.trim().is_empty() {
        return Ok(());
    }
    // The chat is private here, so the chat id is the user id.
    let user_id = msg.chat.id.0;
    if global && !app.is_admin(user_id) {
        bot.send_message(msg.chat.id, "只有管理员能写入全局凭据")
            .await?;
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
    let Some(ext) = app.registry.by_id(&extractor_id) else {
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
            finish_login(
                &bot,
                msg.chat.id,
                &app,
                &dialogue,
                &extractor_id,
                global,
                msg.chat.id.0,
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
    let mut last_hint = None;
    let mut errors = 0;
    for _ in 0..QR_POLL_MAX {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        let Some(ext) = app.registry.by_id(&extractor_id) else {
            break;
        };
        let Some(auth) = ext.auth() else {
            break;
        };
        let progress = auth.advance(&mut session, AuthInput::Poll).await;
        // A newer login or /cancel owns the dialogue now; drop this result.
        if cancel.is_cancelled() {
            return;
        }
        if progress.is_ok() {
            errors = 0;
        }
        match progress {
            Ok(AuthProgress::Pending | AuthProgress::Continue(AuthChallenge::ShowQr { .. })) => {}
            Ok(AuthProgress::Hint(text)) => {
                if last_hint.as_ref() != Some(&text) {
                    let _ = bot.send_message(chat, &text).await;
                    last_hint = Some(text);
                }
            }
            Ok(AuthProgress::Continue(ch)) => {
                let _ = render_challenge(&bot, chat, &ch).await;
            }
            Ok(AuthProgress::Completed(cred)) => {
                if let Err(e) = finish_login(
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
                .await
                {
                    tracing::warn!(error = %e, "failed to save QR login");
                    let _ = bot.send_message(chat, format!("保存登录态失败：{e}")).await;
                }
                return;
            }
            Err(AuthError::SessionExpired) => {
                finish_with(
                    &bot,
                    chat,
                    &dialogue,
                    &cancel,
                    "二维码已过期，请重新 /login。",
                )
                .await;
                return;
            }
            Err(e) => {
                errors += 1;
                tracing::warn!(errors, error = %e, "QR poll failed");
                // Report the first failure only; give up after a few in a row.
                if errors >= QR_POLL_MAX_ERRORS {
                    let text = format!("多次查询扫码状态失败，已退出登录：{e}");
                    finish_with(&bot, chat, &dialogue, &cancel, &text).await;
                    return;
                }
                if errors == 1 {
                    let _ = bot
                        .send_message(chat, format!("查询扫码状态失败：{e}\n会继续重试。"))
                        .await;
                }
            }
        }
    }
    if !cancel.is_cancelled() {
        finish_with(&bot, chat, &dialogue, &cancel, "扫码超时，请重新 /login。").await;
    }
}

/// End a QR login this poller still owns and tell the user why.
async fn finish_with(
    bot: &TgBot,
    chat: ChatId,
    dialogue: &LoginDialogue,
    cancel: &CancellationToken,
    text: &str,
) {
    cancel.cancel();
    dialogue.update(LoginState::Idle).await.ok();
    let _ = bot.send_message(chat, text).await;
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
    let resumed = app
        .store
        .resume_after_login(
            extractor_id,
            (!global).then_some(user_id),
            OffsetDateTime::now_utc(),
        )
        .await?;
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
        .by_id(extractor_id)
        .map(|e| e.display_name().to_string())
        .unwrap_or_else(|| extractor_id.to_string());
    let resumed = if resumed > 0 {
        format!("已恢复 {resumed} 个暂停的订阅。")
    } else {
        String::new()
    };
    bot.send_message(chat, format!("已保存 {name} 登录态。{resumed}{extra}"))
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
    fn parses_global_keyword_anywhere() {
        assert_eq!(parse_target("pixiv"), ("pixiv".into(), false));
        assert_eq!(parse_target("pixiv global"), ("pixiv".into(), true));
        assert_eq!(parse_target("GLOBAL x"), ("x".into(), true));
        // Platform names merely containing the word are not the keyword.
        assert_eq!(parse_target("globalsite"), ("globalsite".into(), false));
    }

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
