//! `/set`: view and edit per-user / per-chat default options.

use feedbot_core::extract::Extractor;
use feedbot_extractors::ExtractorRegistry;
use teloxide::prelude::*;
use teloxide::types::InlineKeyboardMarkup;
use teloxide::utils::html;

use super::{admin_only, is_private, unknown_platform};
use crate::app::App;
use crate::ui::{self, UiCallback};
use feedbot_core::{
    OptionKind, OptionValues, Scope, apply_to_layer, load_saved_options, merge_layers,
    split_option_tokens,
};

pub fn usage_set(registry: &ExtractorRegistry) -> String {
    let platforms = if registry.all().is_empty() {
        "（暂无）".into()
    } else {
        ui::join_blocks(
            &registry
                .all()
                .iter()
                .map(|ext| ui::platform_block(ext.as_ref()))
                .collect::<Vec<_>>(),
        )
    };
    ui::compose(
        "默认参数",
        None,
        &[
            ui::section(
                "用法",
                &format!(
                    "/set {plat} 查看当前默认\n/set {plat} {opts} 修改参数",
                    plat = ui::placeholder("<平台>"),
                    opts = ui::placeholder("[+参数…]"),
                ),
            ),
            ui::section("平台", &platforms),
        ],
    )
}

pub(super) async fn set_opts(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
    rest: String,
) -> anyhow::Result<()> {
    let mut parts = rest.split_whitespace();
    let Some(name) = parts.next() else {
        let plats = ui::named_platforms(app.registry.all(), false);
        ui::send_page(
            bot,
            msg.chat.id,
            usage_set(&app.registry),
            ui::platform_keyboard(UiCallback::Set, &plats),
        )
        .await?;
        return Ok(());
    };
    let Some(ext) = app.registry.resolve_name(name) else {
        ui::send_page(
            bot,
            msg.chat.id,
            unknown_platform(name, &app.registry),
            None,
        )
        .await?;
        return Ok(());
    };
    let rest = parts.collect::<Vec<_>>().join(" ");
    let (tokens, leftover) = split_option_tokens(&rest);
    if !leftover.is_empty() {
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose(
                "已忽略",
                None,
                &[ui::section(
                    "无法识别的片段",
                    &html::escape(&leftover.join(" ")),
                )],
            ),
            None,
        )
        .await?;
    }
    let Some(user) = msg.from.clone() else {
        return Ok(());
    };
    let uid = user.id.0 as i64;
    if tokens.is_empty() {
        let (text, kb) = set_view_page(app, ext.id(), uid, msg.chat.id.0).await?;
        ui::send_page(bot, msg.chat.id, text, kb).await?;
        return Ok(());
    }
    // In groups `/set` edits the chat-wide default, which needs chat admin rights.
    let private = is_private(msg);
    if !app.can_manage_chat(msg.chat.id, uid, private).await {
        ui::send_page(bot, msg.chat.id, admin_only("修改本群默认参数"), None).await?;
        return Ok(());
    }
    let (scope, scope_id) = if private {
        (Scope::User, uid)
    } else {
        (Scope::Chat, msg.chat.id.0)
    };
    let spec = ext.options();
    let layer = app.store.get_platform(scope, scope_id, ext.id()).await?;
    // What this layer currently resolves to: user layers sit above the chat
    // layer, the chat layer only above spec defaults.
    let below = if scope == Scope::User {
        load_saved_options(app.store.as_ref(), ext.id(), uid, msg.chat.id.0).await?
    } else {
        layer.clone()
    };
    let result = merge_layers(spec, &below, &[]).and_then(|effective| {
        let updated = apply_to_layer(spec, &layer, &effective, &tokens)?;
        let mut preview = merge_layers(spec, &updated, &[])?;
        ext.validate(&mut preview)?;
        Ok(updated)
    });
    let updated = match result {
        Ok(v) => v,
        Err(e) => {
            ui::send_page(
                bot,
                msg.chat.id,
                ui::compose(
                    "参数错误",
                    None,
                    &[ui::section("详情", &html::escape(&e.to_string()))],
                ),
                Some(ui::set_view_keyboard(ext.id(), ext.auth().is_some())),
            )
            .await?;
            return Ok(());
        }
    };
    app.store
        .set_platform(scope, scope_id, ext.id(), &updated)
        .await?;
    let lead = if private {
        format!("{} 的个人默认参数", ext.display_name())
    } else {
        format!(
            "{} 的本群默认参数（成员的个人设置优先）",
            ext.display_name()
        )
    };
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(
            "已保存",
            Some(&lead),
            &[ui::section(
                "下一步",
                &format!("/set {} 查看当前取值", ext.id()),
            )],
        ),
        Some(ui::set_view_keyboard(ext.id(), ext.auth().is_some())),
    )
    .await?;
    Ok(())
}

pub(crate) async fn set_view_page(
    app: &App,
    name: &str,
    user_id: i64,
    chat_id: i64,
) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let Some(ext) = app.registry.resolve_name(name) else {
        return Ok((unknown_platform(name, &app.registry), None));
    };
    let saved = load_saved_options(app.store.as_ref(), ext.id(), user_id, chat_id).await?;
    let current = merge_layers(ext.options(), &saved, &[])?;
    Ok((
        format_set_view(ext.as_ref(), &current),
        Some(ui::set_view_keyboard(ext.id(), ext.auth().is_some())),
    ))
}

fn format_set_view(ext: &dyn Extractor, values: &OptionValues) -> String {
    let spec = ext.options();
    let values_body = if spec.options.is_empty() {
        "（此平台没有可配置参数）".into()
    } else {
        spec.options
            .iter()
            .map(|def| match values.get(def.name) {
                Some(v) => format!(
                    "<code>+{}</code> = <code>{}</code>",
                    def.name,
                    html::escape(&v.to_string())
                ),
                None => format!("<code>+{}</code> = （未设）", def.name),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut sections = vec![
        ui::section("取值", &values_body),
        ui::section_expandable("参数", &spec.help_table()),
    ];
    if let Some(example) = set_example(ext) {
        sections.push(ui::section(
            "修改",
            &format!(
                "/set {} {}",
                html::escape(ext.id()),
                ui::placeholder(&example)
            ),
        ));
    }
    ui::compose(ext.display_name(), Some("当前默认"), &sections)
}

fn set_example(ext: &dyn Extractor) -> Option<String> {
    let def = ext.options().options.first()?;
    let token = match def.kind {
        OptionKind::Flag => format!("+{}", def.name),
        OptionKind::Int { min, .. } => format!("+{}:{min}", def.name),
        OptionKind::Str { .. } => format!("+{}:…", def.name),
        OptionKind::Enum { variants } => {
            let v = variants.first().copied().unwrap_or("…");
            format!("+{}:{v}", def.name)
        }
        OptionKind::IntRange { min, max } => format!("+{}:{min}-{max}", def.name),
        OptionKind::List { .. } => format!("+{}:a,b", def.name),
    };
    Some(token)
}
