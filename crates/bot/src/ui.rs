//! Shared HTML layout and inline-keyboard helpers for command replies.

use crate::app::TgBot;
use feedbot_core::extract::Extractor;
use std::sync::Arc;
use teloxide::payloads::{EditMessageTextSetters, SendMessageSetters};
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use teloxide::utils::html;

pub fn title(name: &str) -> String {
    format!("<b>{}</b>", html::escape(name))
}

pub fn title_with_id(name: &str, id: &str) -> String {
    format!("{} ({})", title(name), placeholder(id))
}

pub fn lead(text: &str) -> String {
    html::escape(text)
}

pub fn compose(name: &str, lead_plain: Option<&str>, sections: &[String]) -> String {
    let mut out = title(name);
    if let Some(text) = lead_plain.filter(|s| !s.is_empty()) {
        out.push('\n');
        out.push_str(&lead(text));
    }
    for s in sections {
        out.push_str(s);
    }
    out
}

pub fn section(heading: &str, body: &str) -> String {
    if body.is_empty() {
        return format!("\n<b>{}</b>", html::escape(heading));
    }
    format!(
        "\n<b>{}</b>\n<blockquote>{}</blockquote>",
        html::escape(heading),
        body
    )
}

pub fn section_expandable(heading: &str, body: &str) -> String {
    if body.is_empty() {
        return format!("\n<b>{}</b>", html::escape(heading));
    }
    format!(
        "\n<b>{}</b>\n<blockquote expandable>{}</blockquote>",
        html::escape(heading),
        body
    )
}

pub fn placeholder(s: &str) -> String {
    format!("<code>{}</code>", html::escape(s))
}

pub fn cmd_line(cmd: &str, desc: &str) -> String {
    format!("{} — {}", cmd, html::escape(desc))
}

pub fn platform_block(ext: &dyn Extractor) -> String {
    let auth = if ext.auth().is_some() {
        "需要登录"
    } else {
        "无需登录"
    };
    format!(
        "<code>{}</code>  {}\n{auth}",
        html::escape(ext.id()),
        html::escape(ext.display_name())
    )
}

pub fn join_blocks(blocks: &[String]) -> String {
    blocks.join("\n\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCallback {
    HelpOverview,
    HelpPlatform(String),
    Login(String),
    Set(String),
    Logout(String),
    CancelLogin,
}

pub fn encode(cb: &UiCallback) -> String {
    match cb {
        UiCallback::HelpOverview => "ui:help".into(),
        UiCallback::HelpPlatform(id) => format!("ui:help:{id}"),
        UiCallback::Login(id) => format!("ui:login:{id}"),
        UiCallback::Set(id) => format!("ui:set:{id}"),
        UiCallback::Logout(id) => format!("ui:logout:{id}"),
        UiCallback::CancelLogin => "ui:cancel".into(),
    }
}

pub fn decode(data: &str) -> Option<UiCallback> {
    let rest = data.strip_prefix("ui:")?;
    match rest {
        "help" => return Some(UiCallback::HelpOverview),
        "cancel" => return Some(UiCallback::CancelLogin),
        _ => {}
    }
    let (kind, id) = rest.split_once(':')?;
    if id.is_empty() {
        return None;
    }
    match kind {
        "help" => Some(UiCallback::HelpPlatform(id.into())),
        "login" => Some(UiCallback::Login(id.into())),
        "set" => Some(UiCallback::Set(id.into())),
        "logout" => Some(UiCallback::Logout(id.into())),
        _ => None,
    }
}

pub fn keyboard(rows: Vec<Vec<(String, UiCallback)>>) -> InlineKeyboardMarkup {
    let mapped = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(label, cb)| InlineKeyboardButton::callback(label, encode(&cb)))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    InlineKeyboardMarkup::new(mapped)
}

pub fn platform_keyboard(
    action: fn(String) -> UiCallback,
    platforms: &[(&str, &str)],
) -> Option<InlineKeyboardMarkup> {
    if platforms.is_empty() {
        return None;
    }
    let mut rows = Vec::new();
    for chunk in platforms.chunks(3) {
        let row = chunk
            .iter()
            .map(|(id, label)| ((*label).to_string(), action((*id).to_string())))
            .collect();
        rows.push(row);
    }
    Some(keyboard(rows))
}

pub fn help_platform_keyboard(id: &str, can_login: bool) -> InlineKeyboardMarkup {
    let mut row = vec![("全部命令".into(), UiCallback::HelpOverview)];
    if can_login {
        row.push(("登录".into(), UiCallback::Login(id.into())));
    }
    row.push(("默认参数".into(), UiCallback::Set(id.into())));
    keyboard(vec![row])
}

pub fn set_view_keyboard(id: &str, can_login: bool) -> InlineKeyboardMarkup {
    let mut row = vec![
        ("全部命令".into(), UiCallback::HelpOverview),
        ("平台帮助".into(), UiCallback::HelpPlatform(id.into())),
    ];
    if can_login {
        row.push(("登录".into(), UiCallback::Login(id.into())));
    }
    keyboard(vec![row])
}

pub fn named_platforms(all: &[Arc<dyn Extractor>], auth_only: bool) -> Vec<(&str, &str)> {
    all.iter()
        .filter(|e| !auth_only || e.auth().is_some())
        .map(|e| (e.id(), e.display_name()))
        .collect()
}

pub async fn send_page(
    bot: &TgBot,
    chat_id: ChatId,
    html: impl Into<String>,
    markup: Option<InlineKeyboardMarkup>,
) -> anyhow::Result<()> {
    let mut req = bot.send_message(chat_id, html.into());
    if let Some(kb) = markup {
        req = req.reply_markup(kb);
    }
    req.await?;
    Ok(())
}

pub async fn edit_page(
    bot: &TgBot,
    chat_id: ChatId,
    message_id: MessageId,
    html: impl Into<String>,
    markup: Option<InlineKeyboardMarkup>,
) -> anyhow::Result<()> {
    let mut req = bot.edit_message_text(chat_id, message_id, html.into());
    if let Some(kb) = markup {
        req = req.reply_markup(kb);
    }
    req.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_callback_data() {
        let cases = [
            UiCallback::HelpOverview,
            UiCallback::HelpPlatform("pixiv".into()),
            UiCallback::Login("pixiv".into()),
            UiCallback::Set("pixiv".into()),
            UiCallback::Logout("px".into()),
            UiCallback::CancelLogin,
        ];
        for cb in cases {
            assert_eq!(decode(&encode(&cb)), Some(cb));
        }
        assert_eq!(decode("nope"), None);
        assert_eq!(decode("ui:other"), None);
        assert_eq!(decode("ui:help:"), None);
        assert_eq!(encode(&UiCallback::HelpOverview), "ui:help");
        assert_eq!(encode(&UiCallback::CancelLogin), "ui:cancel");
        assert_eq!(
            encode(&UiCallback::HelpPlatform("pixiv".into())),
            "ui:help:pixiv"
        );
    }

    #[test]
    fn section_uses_blockquote() {
        let html = compose(
            "CongMiaoFeedBot",
            Some("发送链接即可解析。"),
            &[section("平台", "<code>pixiv</code>  Pixiv")],
        );
        assert!(html.starts_with("<b>CongMiaoFeedBot</b>"));
        assert!(html.contains("<blockquote>"));
        assert!(html.contains("<code>pixiv</code>"));
    }
}
