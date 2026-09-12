use feedbot_core::extract::Extractor;
use feedbot_extractors::ExtractorRegistry;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, BotCommandScope, ChatKind, InlineKeyboardMarkup};
use teloxide::utils::command::BotCommands;
use teloxide::utils::html;

use crate::app::App;
use crate::login::{self, LoginDialogue};
use crate::ui::{self, UiCallback};
use feedbot_core::{
    load_saved_options, merge_layers, split_option_tokens, CredentialRepo, JobRepo, OptionKind,
    OptionValues, Scope, SettingsRepo, Subscription, SubscriptionRepo,
};
use time::{Duration as TimeDuration, OffsetDateTime};
use uuid::Uuid;

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
    if !matches!(&cmd, Command::Login(name) if !name.trim().is_empty()) {
        login::abort(&dialogue).await;
    }
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
        Command::Login(name) => login::begin(&bot, &msg, app, dialogue, name).await?,
        Command::Logout(name) if name.trim().is_empty() => {
            logout_usage(&bot, &msg, &app).await?;
        }
        Command::Logout(name) => {
            let (text, kb) =
                logout_html(&app, &name, user_id(&msg), msg.chat.id.0, is_private(&msg)).await?;
            ui::send_page(&bot, msg.chat.id, text, kb).await?;
        }
        Command::Accounts => accounts(&bot, &msg, &app).await?,
        Command::Set(rest) => set_opts(&bot, &msg, &app, rest).await?,
        Command::Sub(rest) => subscribe(&bot, &msg, &app, rest).await?,
        Command::Unsub(rest) => unsubscribe(&bot, &msg, &app, rest).await?,
        Command::Subs => list_subs(&bot, &msg, &app).await?,
        Command::Cancel => {
            login::abort(&dialogue).await;
            let n = app
                .store
                .cancel_queued_for_user(user_id(&msg))
                .await
                .unwrap_or(0);
            ui::send_page(
                &bot,
                msg.chat.id,
                ui::compose(
                    "已取消",
                    Some("已取消登录对话。"),
                    &[ui::section("排队", &format!("丢弃 {n} 个排队任务"))],
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

pub fn help_overview(registry: &ExtractorRegistry) -> String {
    let cmds = Command::bot_commands()
        .into_iter()
        .map(|cmd| {
            let name = cmd.command.trim_start_matches('/');
            ui::cmd_line(&format!("/{name}"), &cmd.description)
        })
        .collect::<Vec<_>>()
        .join("\n");
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
        "CongMiaoFeedBot",
        Some("发送链接即可解析。"),
        &[
            ui::section_expandable("命令", &cmds),
            ui::section("平台", &platforms),
        ],
    )
}

pub fn help_platform_page(
    registry: &ExtractorRegistry,
    name: &str,
) -> (String, Option<InlineKeyboardMarkup>) {
    let Some(ext) = registry.resolve_name(name) else {
        return (unknown_platform(name, registry), None);
    };
    (
        help_platform(registry, name),
        Some(ui::help_platform_keyboard(ext.id(), ext.auth().is_some())),
    )
}

pub fn help_platform(registry: &ExtractorRegistry, name: &str) -> String {
    let Some(ext) = registry.resolve_name(name) else {
        return unknown_platform(name, registry);
    };
    let id = ext.id();
    let lead = if let Some(auth) = ext.auth() {
        let methods: Vec<_> = auth.methods().iter().map(|m| m.to_string()).collect();
        format!("登录方式：{}", methods.join(", "))
    } else {
        "无需登录".into()
    };

    let mut cmds = Vec::new();
    if ext.auth().is_some() {
        cmds.push(ui::cmd_line(&format!("/login {id}"), "登录（仅私聊）"));
        cmds.push(ui::cmd_line(&format!("/logout {id}"), "注销"));
    }
    cmds.push(ui::cmd_line(&format!("/set {id}"), "查看或修改默认参数"));
    cmds.push(ui::cmd_line(
        &format!("/sub {}", ui::placeholder("<链接>")),
        "订阅该来源",
    ));

    let mut kinds_body = String::new();
    let kinds = ext.extract_kinds();
    let urls = ext.url_examples();
    if !kinds.is_empty() {
        kinds_body.push_str(&html::escape(&kinds.join(" / ")));
    }
    if !urls.is_empty() {
        if !kinds_body.is_empty() {
            kinds_body.push('\n');
        }
        kinds_body.push_str("链接：\n");
        kinds_body.push_str(
            &urls
                .iter()
                .map(|u| ui::placeholder(u))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    let mut sections = vec![ui::section("命令", &cmds.join("\n"))];
    if !kinds_body.is_empty() {
        sections.push(ui::section("解析类型", &kinds_body));
    }
    sections.push(ui::section_expandable("参数", &ext.options().help_table()));

    let mut out = ui::title_with_id(ext.display_name(), id);
    out.push('\n');
    out.push_str(&ui::lead(&lead));
    for s in sections {
        out.push_str(&s);
    }
    out
}

pub fn usage_login(registry: &ExtractorRegistry) -> String {
    let logins: Vec<_> = registry
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
    ui::compose(
        "登录",
        Some("私聊选择平台即可开始。"),
        &[
            ui::section(
                "用法",
                &format!("/login {}\n仅私聊可用", ui::placeholder("<平台>")),
            ),
            ui::section("可登录平台", &platforms),
        ],
    )
}

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

pub fn usage_sub() -> String {
    ui::compose(
        "订阅",
        Some("发送该平台支持的链接，可选附加参数。"),
        &[ui::section(
            "用法",
            &format!(
                "/sub {} {}\n参数示例：{}",
                ui::placeholder("<链接>"),
                ui::placeholder("[+参数…]"),
                ui::placeholder("+q:original"),
            ),
        )],
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

async fn logout_usage(bot: &crate::app::TgBot, msg: &Message, app: &App) -> anyhow::Result<()> {
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
                .resolve_name(&meta.extractor_id)
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

pub(crate) async fn logout_html(
    app: &App,
    name: &str,
    user_id: i64,
    chat_id: i64,
    private: bool,
) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let Some(ext) = app.registry.resolve_name(name) else {
        return Ok((unknown_platform(name, &app.registry), None));
    };
    let scope = if private { Scope::User } else { Scope::Chat };
    let scope_id = if scope == Scope::User {
        user_id
    } else {
        chat_id
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

async fn accounts(bot: &crate::app::TgBot, msg: &Message, app: &App) -> anyhow::Result<()> {
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
                "<code>{}</code>  {}/{}\nstatus={}",
                html::escape(&meta.extractor_id),
                meta.scope.as_str(),
                meta.method,
                html::escape(&meta.status)
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

async fn set_opts(
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
    let saved = load_saved_options(
        app.store.as_ref(),
        ext.id(),
        user.id.0 as i64,
        msg.chat.id.0,
    )
    .await?;
    if tokens.is_empty() {
        let current = merge_layers(ext.options(), &saved, &[])?;
        ui::send_page(
            bot,
            msg.chat.id,
            format_set_view(ext.as_ref(), &current),
            Some(ui::set_view_keyboard(ext.id(), ext.auth().is_some())),
        )
        .await?;
        return Ok(());
    }
    let merged = match merge_layers(ext.options(), &saved, &tokens) {
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
    let scope = if is_private(msg) {
        Scope::User
    } else {
        Scope::Chat
    };
    let scope_id = if scope == Scope::User {
        user.id.0 as i64
    } else {
        msg.chat.id.0
    };
    app.store
        .set_platform(scope, scope_id, ext.id(), &merged)
        .await?;
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(
            "已保存",
            Some(&format!("{} 的默认参数", ext.display_name())),
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

async fn subscribe(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
    rest: String,
) -> anyhow::Result<()> {
    if rest.trim().is_empty() {
        ui::send_page(bot, msg.chat.id, usage_sub(), None).await?;
        return Ok(());
    }
    let (tokens, leftover) = split_option_tokens(&rest);
    let url = leftover.iter().find_map(|t| url::Url::parse(t).ok());
    let Some(url) = url.or_else(|| {
        rest.split_whitespace()
            .find_map(|t| url::Url::parse(t).ok())
    }) else {
        ui::send_page(bot, msg.chat.id, usage_sub(), None).await?;
        return Ok(());
    };
    let Some(ext) = app.registry.match_url(&url) else {
        ui::send_page(
            bot,
            msg.chat.id,
            ui::compose(
                "无法订阅",
                Some("没有匹配的解析器。"),
                &[ui::section("下一步", "/help 查看已注册平台")],
            ),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(user) = msg.from.clone() else {
        return Ok(());
    };
    let saved = load_saved_options(
        app.store.as_ref(),
        ext.id(),
        user.id.0 as i64,
        msg.chat.id.0,
    )
    .await?;
    let options = merge_layers(ext.options(), &saved, &tokens)?;
    let now = OffsetDateTime::now_utc();
    let sub = Subscription {
        id: Uuid::new_v4(),
        chat_id: msg.chat.id.0,
        user_id: user.id.0 as i64,
        extractor_id: ext.id().into(),
        source_ref: url.to_string(),
        options,
        cursor: None,
        interval_secs: 3600,
        enabled: true,
        next_due_at: now + TimeDuration::seconds(3600),
        fail_count: 0,
    };
    app.store.insert_subscription(&sub).await?;
    ui::send_page(
        bot,
        msg.chat.id,
        ui::compose(
            "已订阅",
            Some(&format!(
                "{}（首次只记 cursor，不推历史）",
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

async fn unsubscribe(
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

async fn list_subs(bot: &crate::app::TgBot, msg: &Message, app: &App) -> anyhow::Result<()> {
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
                format!(
                    "<code>{}</code>  {}\n{}",
                    s.id,
                    html::escape(&s.extractor_id),
                    html::escape(&s.source_ref)
                )
            })
            .collect::<Vec<_>>(),
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
        let sub = usage_sub();
        assert!(sub.contains("&lt;链接&gt;"));
    }

    #[test]
    fn unknown_platform_suggests() {
        let text = unknown_platform("pixi", &registry());
        assert!(text.contains("pixiv") || text.contains("未知平台"));
    }
}
