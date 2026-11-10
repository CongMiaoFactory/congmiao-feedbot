//! `/sub`, `/unsub`, `/subs`.

use feedbot_extractors::ExtractorRegistry;
use teloxide::prelude::*;
use teloxide::utils::html;

use super::{admin_only, is_private, user_id};
use crate::app::App;
use crate::ui;
use feedbot_core::{Subscription, load_saved_options, split_option_tokens};
use time::OffsetDateTime;
use uuid::Uuid;

pub fn usage_sub(registry: &ExtractorRegistry) -> String {
    ui::compose(
        "订阅",
        Some("发送可订阅来源的链接，可选附加参数。"),
        &[
            ui::section(
                "用法",
                &format!(
                    "/sub {} {}\n参数示例：{}",
                    ui::placeholder("<链接>"),
                    ui::placeholder("[+参数…]"),
                    ui::placeholder("+q:original"),
                ),
            ),
            ui::section("可订阅", &feed_examples(registry)),
        ],
    )
}

pub fn usage_unsub() -> String {
    ui::compose(
        "取消订阅",
        Some("订阅 id 可在 /subs 查看。"),
        &[ui::section(
            "用法",
            &format!("/unsub {}", ui::placeholder("<订阅id>")),
        )],
    )
}

pub(super) async fn subscribe(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
    rest: String,
) -> anyhow::Result<()> {
    let (tokens, leftover) = split_option_tokens(&rest);
    let Some(url) = leftover.iter().find_map(|t| url::Url::parse(t).ok()) else {
        ui::send_page(bot, msg.chat.id, usage_sub(&app.registry), None).await?;
        return Ok(());
    };
    let Some(user) = msg.from.clone() else {
        return Ok(());
    };
    let uid = user.id.0 as i64;
    if !app.can_manage_chat(msg.chat.id, uid, is_private(msg)).await {
        ui::send_page(bot, msg.chat.id, admin_only("管理本群订阅"), None).await?;
        return Ok(());
    }
    let Some(ext) = app.registry.match_feed(&url) else {
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose(
                "无法订阅",
                Some("这个链接不是可订阅的来源。"),
                &[ui::section("可订阅", &feed_examples(&app.registry))],
            ),
            None,
        )
        .await?;
        return Ok(());
    };
    if ext.feed_requires_login() && app.store.resolve(ext.id(), uid).await?.is_none() {
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose(
                "需要登录",
                Some(&format!(
                    "订阅 {} 需要你的登录凭据，请先私聊发送 /login {}。",
                    ext.display_name(),
                    ext.id()
                )),
                &[],
            ),
            None,
        )
        .await?;
        return Ok(());
    }
    // Validate the option text now; it is re-applied to every pushed item.
    let saved = load_saved_options(app.store.as_ref(), ext.id(), uid, msg.chat.id.0).await?;
    if let Err(e) = ext.resolve_options(&saved, &tokens) {
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose(
                "参数错误",
                None,
                &[ui::section("详情", &html::escape(&e.to_string()))],
            ),
            None,
        )
        .await?;
        return Ok(());
    }
    let sub = Subscription {
        id: Uuid::new_v4(),
        chat_id: msg.chat.id.0,
        user_id: uid,
        extractor_id: ext.id().into(),
        source_ref: app.registry.canonicalize(&url).to_string(),
        raw_args: tokens.join(" "),
        cursor: None,
        interval_secs: i64::try_from(app.sub_interval.as_secs()).unwrap_or(i64::MAX),
        enabled: true,
        // Poll soon so the seen set is filled and the source is validated.
        next_due_at: OffsetDateTime::now_utc(),
        fail_count: 0,
        needs_login: false,
    };
    app.store.insert_subscription(&sub).await?;
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(
            "已订阅",
            Some(&format!(
                "{}：首次拉取只记录已有内容，之后有新内容才推送。",
                ext.display_name()
            )),
            &[ui::section(
                "订阅 id",
                &ui::placeholder(&sub.id.to_string()),
            )],
        ),
        None,
    )
    .await?;
    Ok(())
}

fn feed_examples(registry: &ExtractorRegistry) -> String {
    let lines: Vec<String> = registry
        .all()
        .iter()
        .filter(|e| !e.feed_examples().is_empty())
        .map(|e| {
            let shapes = e
                .feed_examples()
                .iter()
                .map(|u| ui::placeholder(u))
                .collect::<Vec<_>>()
                .join("\n");
            let note = if e.feed_requires_login() {
                "（需登录）"
            } else {
                ""
            };
            format!("{}{note}\n{shapes}", html::escape(e.display_name()))
        })
        .collect();
    if lines.is_empty() {
        "（暂无）".into()
    } else {
        ui::join_blocks(&lines)
    }
}

pub(super) async fn unsubscribe(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
    rest: String,
) -> anyhow::Result<()> {
    let trimmed = rest.trim();
    let Ok(id) = Uuid::parse_str(trimmed) else {
        let mut sections = vec![ui::section(
            "用法",
            &format!("/unsub {}", ui::placeholder("<订阅id>")),
        )];
        let list = app.store.list_for_chat(msg.chat.id.0).await?;
        if list.is_empty() {
            sections.push(ui::section("下一步", "/sub 加链接"));
        } else {
            sections.push(ui::section("本会话订阅", &format_sub_list(&list)));
        }
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose("取消订阅", Some("订阅 id 可在 /subs 查看。"), &sections),
            None,
        )
        .await?;
        return Ok(());
    };
    let uid = user_id(msg);
    if !app.can_manage_chat(msg.chat.id, uid, is_private(msg)).await {
        ui::send_page(bot, msg.chat.id, admin_only("管理本群订阅"), None).await?;
        return Ok(());
    }
    let ok = app.store.delete_subscription(id, msg.chat.id.0).await?;
    let (heading, lead, next) = if ok {
        (
            "已取消订阅",
            "该来源不会再推送到本会话。",
            "/subs 查看剩余订阅",
        )
    } else {
        ("没找到该订阅", "请核对订阅 id。", "/subs 查看本会话订阅")
    };
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(heading, Some(lead), &[ui::section("下一步", next)]),
        None,
    )
    .await?;
    Ok(())
}

pub(super) async fn list_subs(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
) -> anyhow::Result<()> {
    let list = app.store.list_for_chat(msg.chat.id.0).await?;
    if list.is_empty() {
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose(
                "订阅",
                Some("当前没有订阅。"),
                &[ui::section("下一步", "/sub 加链接")],
            ),
            None,
        )
        .await?;
        return Ok(());
    }
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(
            "订阅",
            None,
            &[
                ui::section("本会话", &format_sub_list(&list)),
                ui::section("取消", &format!("/unsub {}", ui::placeholder("<订阅id>"))),
            ],
        ),
        None,
    )
    .await?;
    Ok(())
}

fn format_sub_list(list: &[Subscription]) -> String {
    ui::join_blocks(
        &list
            .iter()
            .map(|s| {
                let status = if !s.enabled {
                    "  （已停用）".to_string()
                } else if s.needs_login {
                    format!("  （需重新登录：/login {}）", html::escape(&s.extractor_id))
                } else {
                    String::new()
                };
                format!(
                    "<code>{}</code>  {}{status}\n{}",
                    s.id,
                    html::escape(&s.extractor_id),
                    html::escape(&s.source_ref)
                )
            })
            .collect::<Vec<_>>(),
    )
}
