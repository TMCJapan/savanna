use std::env;

use linkify::{LinkFinder, LinkKind};
use serenity::all::{Context, EventHandler, GatewayIntents, Message, Ready};
use serenity::{Client, async_trait};
use url::Url;

/// Discord のメッセージ URL から guild id を取り出す。
/// 対象外の URL や DM (`/channels/@me/...`) の場合は `None` を返す。
fn url_filter<'a>(link: linkify::Link<'a>) -> Option<u64> {
    let url = Url::parse(link.as_str()).unwrap();
    if matches!(
        url.host(),
        Some(url::Host::Domain("discord.com" | "discordapp.com"))
    ) {
        let mut segments = url.path_segments().unwrap();
        if Some("channels") == segments.next()
            && let Some(raw_guild_id) = segments.next()
            && let Ok(guild_id) = raw_guild_id.parse::<u64>()
        {
            return Some(guild_id);
        }
    }
    None
}

struct Handler {
    finder: LinkFinder,
}

impl Handler {
    fn new() -> Self {
        let mut finder = LinkFinder::new();
        // メールアドレスを拾うと url_filter 内の Url::parse が panic するため URL のみに限定する
        finder.kinds(&[LinkKind::Url]);
        Self { finder }
    }

    /// メッセージ本文から guild id を重複なく (出現順で) 抽出する
    fn extract_guild_ids(&self, content: &str) -> Vec<u64> {
        let mut guild_ids = Vec::new();
        for guild_id in self.finder.links(content).filter_map(url_filter) {
            if !guild_ids.contains(&guild_id) {
                guild_ids.push(guild_id);
            }
        }
        guild_ids
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        // bot 自身を含む bot の発言には反応しない
        if msg.author.bot {
            return;
        }

        let guild_ids = self.extract_guild_ids(&msg.content);
        if guild_ids.is_empty() {
            return;
        }

        let body = guild_ids
            .iter()
            .map(|guild_id| format!("- `{guild_id}`"))
            .collect::<Vec<_>>()
            .join("\n");

        if let Err(why) = msg.reply(&ctx.http, body).await {
            eprintln!("返信に失敗しました: {why:?}");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        println!("{} として接続しました", ready.user.name);
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let token = env::var("DISCORD_TOKEN").expect("環境変数 DISCORD_TOKEN が設定されていません");

    // MESSAGE_CONTENT は特権インテントなので Developer Portal 側での有効化も必要
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler::new())
        .await
        .expect("クライアントの作成に失敗しました");

    if let Err(why) = client.start().await {
        eprintln!("クライアントの起動に失敗しました: {why:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::Handler;

    fn extract(content: &str) -> Vec<u64> {
        Handler::new().extract_guild_ids(content)
    }

    #[test]
    fn メッセージurlからguild_idを抽出する() {
        assert_eq!(
            extract("https://discord.com/channels/123/456/789"),
            vec![123]
        );
    }

    #[test]
    fn discordapp_comも対象にする() {
        assert_eq!(
            extract("https://discordapp.com/channels/123/456/789"),
            vec![123]
        );
    }

    #[test]
    fn dmのurlは無視する() {
        assert!(extract("https://discord.com/channels/@me/456/789").is_empty());
    }

    #[test]
    fn discord以外のurlは無視する() {
        assert!(extract("https://example.com/channels/123/456/789").is_empty());
    }

    #[test]
    fn canaryなどのサブドメインは対象外() {
        assert!(extract("https://canary.discord.com/channels/123/456/789").is_empty());
    }

    #[test]
    fn メールアドレスが含まれていてもpanicしない() {
        assert_eq!(
            extract("foo@example.com https://discord.com/channels/123/456/789"),
            vec![123]
        );
    }

    #[test]
    fn 複数のurlを出現順に重複なく抽出する() {
        let content = "https://discord.com/channels/111/1/1 と \
             https://discord.com/channels/222/2/2 と \
             https://discord.com/channels/111/3/3";
        assert_eq!(extract(content), vec![111, 222]);
    }

    #[test]
    fn 埋め込み抑制の山括弧付きurlも抽出できる() {
        assert_eq!(
            extract("<https://discord.com/channels/123/456/789>"),
            vec![123]
        );
    }

    #[test]
    fn urlがなければ空になる() {
        assert!(extract("ただのテキスト").is_empty());
    }
}
