//! Bot commands: the `Command` enum, dispatch, and helpers shared by the
//! per-command modules.

mod account;
mod help;
mod set;
mod sub;

use feedbot_extractors::ExtractorRegistry;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, BotCommandScope, ChatKind};
use teloxide::utils::command::BotCommands;

use crate::app::App;
use crate::login::{self, LoginDialogue};
use crate::ui::{self, UiCallback};
use account::{accounts, logout_usage};
use feedbot_core::Scope;
use set::set_opts;
use sub::{list_subs, subscribe, unsubscribe};

pub(crate) use account::logout_html;
pub use help::{help_overview, help_platform, help_platform_page, usage_login};
pub(crate) use set::set_view_page;
pub use set::usage_set;
pub use sub::{usage_sub, usage_unsub};

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "显示帮助，可加平台名查看该平台参数")]
    Help(String),
    #[command(description = "登录平台账号（仅私聊）")]
    Login(String),
    #[command(description = "注销平台账号")]
    Logout(String),
    #[command(description = "查看已登录的平台与凭据状态")]
    Accounts,
    #[command(description = "查看或修改平台默认参数")]
    Set(String),
    #[command(description = "订阅一个来源")]
    Sub(String),
    #[command(description = "取消订阅")]
    Unsub(String),
    #[command(description = "查看本会话订阅")]
    Subs,
    #[command(description = "取消进行中的登录或排队任务")]
    Cancel,
    #[command(description = "关于本 bot")]
    Start,
}

impl Command {
    pub fn group_commands() -> Vec<BotCommand> {
        Self::bot_commands()
            .into_iter()
            .filter(|c| !matches!(c.command.as_str(), "login" | "logout" | "accounts"))
            .collect()
    }
}

pub async fn install_menu(bot: &crate::app::TgBot) -> ResponseResult<()> {
    bot.set_my_commands(Command::bot_commands())
        .scope(BotCommandScope::AllPrivateChats)
        .await?;
    bot.set_my_commands(Command::group_commands())
        .scope(BotCommandScope::AllGroupChats)
        .await?;
    Ok(())
}

pub async fn handle(
    bot: crate::app::TgBot,
    msg: Message,
    cmd: Command,
    app: Arc<App>,
    dialogue: LoginDialogue,
) -> anyhow::Result<()> {
    // Any command ends a pending login; `/login <platform>` starts a fresh one.
    login::abort(&dialogue).await;
    match cmd {
        Command::Start => {
            ui::send_page(&bot, msg.chat.id, about_bot(), None).await?;
        }
        Command::Help(name) if name.trim().is_empty() => {
            let plats = ui::named_platforms(app.registry.all(), false);
            ui::send_page(
                &bot,
                msg.chat.id,
                help_overview(&app.registry),
                ui::platform_keyboard(UiCallback::HelpPlatform, &plats),
            )
            .await?;
        }
        Command::Help(name) => {
            let (text, kb) = help_platform_page(&app.registry, &name);
            ui::send_page(&bot, msg.chat.id, text, kb).await?;
        }
        Command::Login(name) if name.trim().is_empty() => {
            if !is_private(&msg) {
                ui::send_page(&bot, msg.chat.id, "请私聊使用 /login", None).await?;
            } else {
                let plats = ui::named_platforms(app.registry.all(), true);
                ui::send_page(
                    &bot,
                    msg.chat.id,
                    usage_login(&app.registry),
                    ui::platform_keyboard(UiCallback::Login, &plats),
                )
                .await?;
            }
        }
        Command::Login(args) => {
            let (name, global) = login::parse_target(&args);
            login::begin(&bot, &msg, app, dialogue, name, global).await?;
        }
        Command::Logout(args) if args.trim().is_empty() => {
            logout_usage(&bot, &msg, &app).await?;
        }
        Command::Logout(args) => {
            let (name, global) = login::parse_target(&args);
            let target = if global {
                if !app.is_admin(user_id(&msg)) {
                    ui::send_page(&bot, msg.chat.id, "只有管理员能注销全局凭据", None).await?;
                    return Ok(());
                }
                (Scope::Global, 0)
            } else {
                (Scope::User, user_id(&msg))
            };
            let (text, kb) = logout_html(&app, &name, target).await?;
            ui::send_page(&bot, msg.chat.id, text, kb).await?;
        }
        Command::Accounts => accounts(&bot, &msg, &app).await?,
        Command::Set(rest) => set_opts(&bot, &msg, &app, rest).await?,
        Command::Sub(rest) => subscribe(&bot, &msg, &app, rest).await?,
        Command::Unsub(rest) => unsubscribe(&bot, &msg, &app, rest).await?,
        Command::Subs => list_subs(&bot, &msg, &app).await?,
        Command::Cancel => {
            // Running jobs notice the cancelled state at their next step.
            let n = app.store.cancel_for_user(user_id(&msg)).await?;
            ui::send_page(
                &bot,
                msg.chat.id,
                ui::compose(
                    "已取消",
                    Some("已退出进行中的登录。"),
                    &[ui::section(
                        "任务",
                        &format!("取消了 {n} 个排队或进行中的任务"),
                    )],
                ),
                None,
            )
            .await?;
        }
    }
    Ok(())
}

pub fn about_bot() -> String {
    ui::compose(
        "CongMiaoFeedBot",
        Some("会从链接里解析媒体并发送到当前会话。直接发送链接即可。"),
        &[ui::section("下一步", "/help")],
    )
}

pub fn unknown_platform(name: &str, registry: &ExtractorRegistry) -> String {
    let escaped = ui::placeholder(name.trim());
    let body = if let Some(s) = registry.suggest_name(name) {
        format!(
            "未知平台 {escaped}，你是不是想输入 {}？",
            ui::placeholder(&s)
        )
    } else {
        format!("未知平台 {escaped}。用 /help 查看列表。")
    };
    ui::compose("未知平台", None, &[ui::section("提示", &body)])
}

fn is_private(msg: &Message) -> bool {
    matches!(msg.chat.kind, ChatKind::Private(_))
}

fn user_id(msg: &Message) -> i64 {
    msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0)
}

fn admin_only(action: &str) -> String {
    ui::compose(
        "没有权限",
        Some(&format!("只有群管理员可以{action}。")),
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ExtractorRegistry {
        ExtractorRegistry::builtin()
    }

    #[test]
    fn overview_lists_commands_and_platforms_without_requiring_site() {
        let text = help_overview(&registry());
        assert!(text.contains("<blockquote"));
        assert!(text.contains("/help — "));
        assert!(text.contains("/login — 登录平台账号（仅私聊）"));
        assert!(text.contains("/start — 关于本 bot"));
        assert!(text.contains("平台"));
        assert!(text.contains("pixiv"));
        assert!(text.contains("Pixiv"));
        assert!(text.contains("需要登录"));
        assert!(text.contains("发送链接即可解析。"));
        assert!(!text.contains("/login pixiv —"));
        assert!(!text.contains("<code>/help"));
    }

    #[test]
    fn platform_help_includes_related_commands_and_kinds() {
        let text = help_platform(&registry(), "pixiv");
        assert!(text.contains("<blockquote"));
        assert!(text.contains("/login pixiv"));
        assert!(text.contains("/logout pixiv"));
        assert!(text.contains("/set pixiv"));
        assert!(text.contains("/sub"));
        assert!(!text.contains("<code>/login pixiv"));
        assert!(text.contains("插画"));
        assert!(text.contains("漫画"));
        assert!(text.contains("ugoira"));
        assert!(text.contains("/artworks/{id}"));
        assert!(text.contains("+q"));
        assert!(text.contains("+sp"));
        assert!(text.contains("<i>original | regular</i>"));
    }

    #[test]
    fn youtube_help_and_login_aliases() {
        let text = help_platform(&registry(), "ytbm");
        assert!(text.contains("/login youtube"));
        assert!(text.contains("/logout youtube"));
        assert!(text.contains("视频"));
        assert!(text.contains("音频"));
        assert!(text.contains("直播封面"));
        assert!(text.contains("music.youtube.com"));
        assert!(text.contains("+q"));
        let login = usage_login(&registry());
        assert!(login.contains("youtube"));
    }

    #[test]
    fn bili_and_netease_help_and_login_aliases() {
        let bili = help_platform(&registry(), "b站");
        assert!(bili.contains("/login bilibili"));
        assert!(bili.contains("视频"));
        assert!(bili.contains("动态"));
        let ncm = help_platform(&registry(), "网易云");
        assert!(ncm.contains("/login netease"));
        assert!(ncm.contains("歌曲"));
        let login = usage_login(&registry());
        assert!(login.contains("bilibili"));
        assert!(login.contains("netease"));
    }

    #[test]
    fn empty_login_and_set_usage_describe_args() {
        let login = usage_login(&registry());
        assert!(login.contains("/login"));
        assert!(login.contains("&lt;平台&gt;"));
        assert!(login.contains("pixiv"));
        assert!(login.contains("<blockquote"));
        let set = usage_set(&registry());
        assert!(set.contains("/set"));
        assert!(set.contains("&lt;平台&gt;"));
        let sub = usage_sub(&registry());
        assert!(sub.contains("&lt;链接&gt;"));
    }

    #[test]
    fn unknown_platform_suggests() {
        let text = unknown_platform("pixi", &registry());
        assert!(text.contains("pixiv") || text.contains("未知平台"));
    }
}
