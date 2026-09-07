use std::env;
use std::time::Duration;

use linkify::{LinkFinder, LinkKind};
use serde::Deserialize;
use serenity::all::{
    Colour, Context, CreateAllowedMentions, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter,
    CreateMessage, EventHandler, GatewayIntents, GuildId, GuildPreview, Http, HttpError, ImageHash,
    Message, Ready, StatusCode,
};
use serenity::futures::future::join_all;
use serenity::{Client, async_trait};
use url::Url;

/// 1 メッセージに載せられる embed の上限 (Discord 側の制約)
const MAX_EMBEDS: usize = 10;

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

/// ギルドアイコンの CDN URL を組み立てる
fn icon_url(guild_id: GuildId, icon: Option<&ImageHash>) -> Option<String> {
    icon.map(|icon| {
        let ext = if icon.is_animated() { "gif" } else { "png" };
        format!("https://cdn.discordapp.com/icons/{guild_id}/{icon}.{ext}?size=256")
    })
}

/// `GET /guilds/{id}/widget.json` の結果
enum Invite {
    Found(String),
    /// ウィジェットが無効 (403) か、招待リンクが設定されていない
    Unavailable,
    /// 通信エラーなど、判定不能な失敗
    Failed,
}

/// `GET /guilds/{id}/preview` の結果
enum Preview {
    Found(Box<GuildPreview>),
    /// Bot が未参加で、かつ Server Discovery にも未登録 (404)
    Unavailable,
    /// レート制限や障害など、判定不能な失敗
    Failed(String),
}

/// サーバーのメタデータを取得する。
/// Bot が参加しているか、Server Discovery に登録されているサーバーのみ成功する。
async fn fetch_preview(http: &Http, guild_id: GuildId) -> Preview {
    match http.get_guild_preview(guild_id).await {
        Ok(preview) => Preview::Found(Box::new(preview)),
        Err(serenity::Error::Http(HttpError::UnsuccessfulRequest(response)))
            if response.status_code == StatusCode::NOT_FOUND =>
        {
            Preview::Unavailable
        }
        Err(why) => Preview::Failed(why.to_string()),
    }
}

/// 招待リンクを取得する。
/// `GET /guilds/{id}/widget.json` は認証不要の公開エンドポイントで、
/// ウィジェットが有効なサーバーだけが招待リンクを返す (無効なら 403)。
async fn fetch_invite(client: &reqwest::Client, guild_id: GuildId) -> Invite {
    #[derive(Deserialize)]
    struct GuildWidget {
        instant_invite: Option<String>,
    }

    let url = format!("https://discord.com/api/v10/guilds/{guild_id}/widget.json");
    let Ok(response) = client.get(url).send().await else {
        return Invite::Failed;
    };
    let status = response.status();
    if !status.is_success() {
        // ウィジェット無効は 403、存在しないサーバーは 404
        return match status {
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Invite::Unavailable,
            _ => Invite::Failed,
        };
    }
    match response.json::<GuildWidget>().await {
        Ok(widget) => widget
            .instant_invite
            .map_or(Invite::Unavailable, Invite::Found),
        Err(_) => Invite::Failed,
    }
}

/// 取得したメタデータを embed に整形する
fn guild_embed(guild_id: GuildId, preview: &GuildPreview, invite: &Invite) -> CreateEmbed {
    let mut auther = CreateEmbedAuthor::new(&preview.name);
    if let Invite::Found(url) = invite {
        auther = auther.url(url);
    }
    if let Some(icon) = icon_url(guild_id, preview.icon.as_ref()) {
        auther = auther.icon_url(icon);
    }

    CreateEmbed::new().colour(Colour::BLURPLE).author(auther)
}

/// 取得できなかった場合の embed
fn error_embed(guild_id: GuildId, reason: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new()
        .title("サーバー情報を取得できませんでした")
        .colour(Colour::RED)
        .description(reason)
        .footer(CreateEmbedFooter::new(format!("Guild ID: {guild_id}")))
}

struct Handler {
    finder: LinkFinder,
    client: reqwest::Client,
}

impl Handler {
    fn new() -> Self {
        let mut finder = LinkFinder::new();
        // メールアドレスを拾うと url_filter 内の Url::parse が panic するため URL のみに限定する
        finder.kinds(&[LinkKind::Url]);
        // ウィジェットの取得が詰まってもメッセージ処理を止めないよう短めに打ち切る
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("HTTP クライアントの作成に失敗しました");
        Self { finder, client }
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

    async fn build_embed(&self, ctx: &Context, guild_id: GuildId) -> CreateEmbed {
        let (preview, invite) = tokio::join!(
            fetch_preview(&ctx.http, guild_id),
            fetch_invite(&self.client, guild_id),
        );

        match preview {
            Preview::Found(preview) => guild_embed(guild_id, &preview, &invite),
            Preview::Unavailable => error_embed(
                guild_id,
                "Bot が参加しておらず、Server Discovery にも未登録のため取得できませんでした",
            ),
            Preview::Failed(why) => {
                error_embed(guild_id, format!("取得に失敗しました\n```\n{why}\n```"))
            }
        }
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

        let embeds = join_all(
            guild_ids
                .iter()
                .take(MAX_EMBEDS)
                .map(|guild_id| self.build_embed(&ctx, GuildId::new(*guild_id))),
        )
        .await;

        let mut reply = CreateMessage::new()
            .embeds(embeds)
            .reference_message(&msg)
            .allowed_mentions(CreateAllowedMentions::new().replied_user(false));
        if guild_ids.len() > MAX_EMBEDS {
            reply = reply.content(format!(
                "{} 件見つかりましたが、先頭 {MAX_EMBEDS} 件のみ表示します",
                guild_ids.len()
            ));
        }

        if let Err(why) = msg.channel_id.send_message(&ctx.http, reply).await {
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
    use super::{Handler, icon_url};
    use serenity::all::{GuildId, ImageHash};

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

    #[test]
    fn アイコンがなければurlを組み立てない() {
        assert_eq!(icon_url(GuildId::new(123), None), None);
    }

    #[test]
    fn 静止画アイコンはpngで組み立てる() {
        let hash: ImageHash = "0123456789abcdef0123456789abcdef".parse().unwrap();
        assert_eq!(
            icon_url(GuildId::new(123), Some(&hash)).unwrap(),
            "https://cdn.discordapp.com/icons/123/0123456789abcdef0123456789abcdef.png?size=256"
        );
    }

    #[test]
    fn アニメーションアイコンはgifで組み立てる() {
        let hash: ImageHash = "a_0123456789abcdef0123456789abcdef".parse().unwrap();
        assert_eq!(
            icon_url(GuildId::new(123), Some(&hash)).unwrap(),
            "https://cdn.discordapp.com/icons/123/a_0123456789abcdef0123456789abcdef.gif?size=256"
        );
    }
}
