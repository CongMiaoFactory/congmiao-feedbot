//! `/logout` and `/accounts`.

use teloxide::prelude::*;
use teloxide::types::InlineKeyboardMarkup;
use teloxide::utils::html;

use super::{unknown_platform, user_id};
use crate::app::App;
use crate::ui::{self, UiCallback};
use feedbot_core::{CredentialStatus, Scope};

pub(super) async fn logout_usage(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
) -> anyhow::Result<()> {
    let logged = app
        .store
        .list_for_user(user_id(msg))
        .await
        .unwrap_or_default();
    let (body, kb) = if logged.is_empty() {
        let logins: Vec<_> = app
            .registry
            .all()
            .iter()
            .filter(|e| e.auth().is_some())
            .collect();
        let platforms = if logins.is_empty() {
            "当前没有需要登录的平台。".into()
        } else {
            ui::join_blocks(
                &logins
                    .iter()
                    .map(|ext| ui::platform_block(ext.as_ref()))
                    .collect::<Vec<_>>(),
            )
        };
        let plats = ui::named_platforms(app.registry.all(), true);
        (platforms, ui::platform_keyboard(UiCallback::Logout, &plats))
    } else {
        let mut owned = Vec::new();
        let mut lines = Vec::new();
        for meta in &logged {
            let label = app
                .registry
                .by_id(&meta.extractor_id)
                .map(|e| e.display_name().to_string())
                .unwrap_or_else(|| meta.extractor_id.clone());
            lines.push(format!(
                "<code>{}</code>  {}\n{}/{}",
                html::escape(&meta.extractor_id),
                html::escape(&label),
                meta.scope.as_str(),
                meta.method
            ));
            owned.push((meta.extractor_id.clone(), label));
        }
        let refs: Vec<_> = owned
            .iter()
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect();
        (
            ui::join_blocks(&lines),
            ui::platform_keyboard(UiCallback::Logout, &refs),
        )
    };
    let heading = if logged.is_empty() {
        "可注销平台"
    } else {
        "已登录"
    };
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(
            "注销",
            None,
            &[
                ui::section("用法", &format!("/logout {}", ui::placeholder("<平台>"))),
                ui::section(heading, &body),
            ],
        ),
        kb,
    )
    .await?;
    Ok(())
}

/// Delete the credential stored at `(scope, scope_id)` for platform `name`.
pub(crate) async fn logout_html(
    app: &App,
    name: &str,
    (scope, scope_id): (Scope, i64),
) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let Some(ext) = app.registry.resolve_name(name) else {
        return Ok((unknown_platform(name, &app.registry), None));
    };
    let ok = app
        .store
        .delete_credential(ext.id(), scope, scope_id)
        .await?;
    let text = if ok {
        ui::compose(
            "已注销",
            Some(ext.display_name()),
            &[ui::section("下一步", "/login 重新登录，或 /help 查看命令")],
        )
    } else {
        ui::compose(
            "未找到凭据",
            Some("没有找到对应凭据。"),
            &[ui::section("下一步", "/login 登录后再试")],
        )
    };
    Ok((
        text,
        Some(ui::help_platform_keyboard(ext.id(), ext.auth().is_some())),
    ))
}

pub(super) async fn accounts(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
) -> anyhow::Result<()> {
    let Some(user) = msg.from.clone() else {
        return Ok(());
    };
    let list = app.store.list_for_user(user.id.0 as i64).await?;
    if list.is_empty() {
        let plats = ui::named_platforms(app.registry.all(), true);
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose(
                "凭据",
                Some("还没有保存的凭据。"),
                &[ui::section(
                    "下一步",
                    &format!("私聊 /login {}", ui::placeholder("<平台>")),
                )],
            ),
            ui::platform_keyboard(UiCallback::Login, &plats),
        )
        .await?;
        return Ok(());
    }
    let body = list
        .iter()
        .map(|meta| {
            format!(
                "<code>{}</code>  {}/{}\n{}",
                html::escape(&meta.extractor_id),
                meta.scope.as_str(),
                meta.method,
                status_label(meta.status)
            )
        })
        .collect::<Vec<_>>();
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(
            "凭据",
            None,
            &[ui::section("已保存", &ui::join_blocks(&body))],
        ),
        None,
    )
    .await?;
    Ok(())
}

fn status_label(status: CredentialStatus) -> &'static str {
    match status {
        CredentialStatus::Valid => "有效",
        CredentialStatus::Expired => "已过期，请重新 /login",
        CredentialStatus::Invalid => "已失效，请重新 /login",
        CredentialStatus::Unknown => "状态未知",
    }
}
