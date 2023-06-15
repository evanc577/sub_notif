use std::io::ErrorKind;
use std::time::Duration;

use anyhow::Result;
use futures::stream::StreamExt;
use reddit_api::structs::{Post, SubredditSort};
use reddit_api::RedditClient;
use serde::Deserialize;
use time::format_description::well_known::Iso8601;
use time::OffsetDateTime;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::MissedTickBehavior;

const LAST_ID_FILE: &str = "last_seen.txt";
const CONFIG_FILE: &str = "config.yaml";
const REQ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const NUM_POSTS: usize = 50;

#[derive(Deserialize, Debug)]
struct Config {
    pushover_token: String,
    pushover_user: String,
    subreddit: String,
}

#[tokio::main]
async fn main() {
    // parse config file
    let config = &parse_config().await;
    let subreddit = &config.subreddit;
    let reddit_client = RedditClient::new().unwrap();
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(REQ_TIMEOUT)
        .build()
        .unwrap();
    let mut last_id = last_id().await.unwrap();

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;

        // get recent posts
        let posts = match reddit_posts(&reddit_client, subreddit, &last_id).await {
            Ok(posts) => posts,
            Err(_) => continue,
        };

        // send notifications via pushover
        if !posts.is_empty() {
            match pushover(config, &client, &posts).await {
                Ok(_) => {
                    match parse_id(&posts[0].id) {
                        Ok(id) => last_id = Some(id),
                        Err(_) => eprintln!("Invalid post id {}", &posts[0].id),
                    }
                }
                Err(e) => eprintln!("Pushover error: {}", e),
            }
        }
    }
}

async fn parse_config() -> Config {
    let contents = fs::read_to_string(CONFIG_FILE).await.unwrap_or_else(|_| {
        eprintln!("Error: failed to open file {}", CONFIG_FILE);
        panic!();
    });

    serde_yaml::from_str(&contents).unwrap_or_else(|err| {
        eprintln!("Error parsing {}: {:?}", CONFIG_FILE, err);
        panic!();
    })
}

async fn last_id() -> Result<Option<u64>> {
    let mut f = match fs::File::open(LAST_ID_FILE).await {
        Ok(f) => f,
        Err(e) => {
            if e.kind() == ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(e.into());
        }
    };

    let mut last_id = String::new();
    f.read_to_string(&mut last_id).await?;
    Ok(Some(parse_id(&last_id)?))
}

async fn reddit_posts(
    client: &RedditClient,
    subreddit: &str,
    last_id: &Option<u64>,
) -> Result<Vec<Post>> {
    let query = client
        .subreddit_posts_query()
        .subreddit(subreddit)
        .sort(SubredditSort::New)
        .build();
    let mut stream = query.execute().await.take(NUM_POSTS);
    let mut posts = Vec::new();
    while let Some(post) = stream.next().await {
        let post = post?;
        if let Some(last_id) = last_id {
            if parse_id(&post.id)? <= *last_id {
                break;
            }
        }
        posts.push(post);
    }
    Ok(posts)
}

fn parse_id(id: &str) -> Result<u64> {
    Ok(u64::from_str_radix(id.trim().trim_start_matches("t3_"), 36)?)
}

async fn pushover(config: &Config, client: &reqwest::Client, posts: &[Post]) -> Result<()> {
    const POST_TIMEOUT_RETRY: i32 = 3;

    for post in posts.iter().rev() {
        for attempt in 0..POST_TIMEOUT_RETRY {
            // set parameters
            let timestamp =
                OffsetDateTime::parse(&post.created_at, &Iso8601::DEFAULT)?.unix_timestamp();
            let decoded_title = htmlescape::decode_html(&post.title).unwrap_or(post.title.clone());
            let params = [
                ("token", &config.pushover_token),
                ("user", &config.pushover_user),
                ("title", &format!("New post on r/{}", "dreamcatcher")),
                ("message", &decoded_title),
                ("url", &format!("https://redd.it/{}", post.id.trim_start_matches("t3_"))),
                ("timestamp", &timestamp.to_string()),
            ];

            // send POST
            let resp = client
                .post("https://api.pushover.net/1/messages.json")
                .form(&params)
                .send()
                .await;

            // check if POST is ok
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    if e.is_timeout() {
                        // retry if timed out
                        eprintln!(
                            "POST {} to pushover timed out (attempt {} of {})",
                            post.id,
                            attempt + 1,
                            POST_TIMEOUT_RETRY
                        );
                        continue;
                    } else {
                        // break if failed for other reason
                        eprintln!("{:?}", e);
                        break;
                    }
                }
            };

            // record last successful push
            if resp.status().is_success() {
                match resp.text().await {
                    Err(e) => eprintln!("{:?}", e), // could not parse resp body
                    Ok(_) => {
                        let mut f = fs::File::create(LAST_ID_FILE).await.unwrap();
                        f.write_all(post.id.as_bytes()).await.unwrap();
                        println!("{}", &post.id);
                    }
                }
            }

            break;
        }
    }

    Ok(())
}
