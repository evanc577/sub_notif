use anyhow::Result;
use lazy_static::lazy_static;
use reqwest::Url;
use serde::Deserialize;
use unqlite::{Transaction, UnQLite, KV};

const SEEN_DB: &str = "seen.udb";
const CONFIG_FILE: &str = "config.yaml";
const REQ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const NUM_POSTS: usize = 50;

lazy_static! {
    static ref DB: unqlite::UnQLite = UnQLite::create(&SEEN_DB);
    static ref CLIENT: reqwest::Client = reqwest::Client::builder()
        .timeout(REQ_TIMEOUT)
        .build()
        .unwrap();
    static ref R_CLIENT: reqwest::Client = reqwest::Client::builder()
        .timeout(REQ_TIMEOUT)
        .user_agent("sub notifier (by u/test241894)")
        .build()
        .unwrap();
}

#[derive(Deserialize, Debug)]
struct CONFIG {
    pushover_token: String,
    pushover_user: String,
    subreddit: String,
}

#[derive(Deserialize, Debug)]
struct POResp {
    status: i64,
}

#[derive(Deserialize, Debug)]
struct PSResp {
    data: Vec<Post>,
}

#[derive(Deserialize, Debug)]
struct RedditResp {
    data: RedditRespData,
}

#[derive(Deserialize, Debug)]
struct RedditRespData {
    children: Vec<RedditChildren>,
}

#[derive(Deserialize, Debug)]
struct RedditChildren {
    data: Post,
}

#[derive(Deserialize, Debug)]
struct RedditChildrenData {
    post: Post,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum StringOrF64 {
    String(String),
    F64(f64),
}

#[derive(Deserialize, Debug)]
struct Post {
    title: String,
    id: String,
    created_utc: StringOrF64,
}

#[tokio::main]
async fn main() {
    // parse config file
    let config = &parse_config();

    let subreddit = &config.subreddit;

    loop {
        let start = tokio::time::Instant::now();

        // get recent posts
        let r_fut = reddit(subreddit);
        let ps_fut = pushshift(subreddit);
        let (r_result, po_result) = tokio::join!(r_fut, ps_fut);
        let r_posts = r_result.unwrap_or_else(|err| {
            eprintln!("Reddit error: {}", err);
            Vec::new()
        });
        let ps_posts = po_result.unwrap_or_else(|err| {
            eprintln!("Pushshift error: {}", err);
            Vec::new()
        });

        // concatenate posts
        let posts = r_posts
            .into_iter()
            .chain(ps_posts.into_iter())
            .collect::<Vec<_>>();

        // send notifications via pushover
        if !posts.is_empty() {
            pushover(config, posts)
                .await
                .unwrap_or_else(|err| eprintln!("Pushover error: {}", err));
        }

        // sleep for a while
        tokio::time::sleep_until(start + tokio::time::Duration::from_secs(10)).await;
    }
}

fn parse_config() -> CONFIG {
    let contents = std::fs::read_to_string(&CONFIG_FILE).unwrap_or_else(|_| {
        eprintln!("Error: failed to open file {}", &CONFIG_FILE);
        panic!();
    });

    serde_yaml::from_str(&contents).unwrap_or_else(|err| {
        eprintln!("Error parsing {}: {:?}", &CONFIG_FILE, err);
        panic!();
    })
}

async fn pushshift(subreddit: &str) -> Result<Vec<Post>> {
    let pushshift_url = Url::parse_with_params(
        "https://api.pushshift.io/reddit/submission/search",
        &[
            ("subreddit", subreddit),
            ("size", NUM_POSTS.to_string().as_str()),
        ],
    )?;

    let resp = CLIENT.get(pushshift_url).send().await?;

    let json: PSResp = serde_json::from_str(&resp.text().await?.as_str())?;

    Ok(json.data)
}

async fn reddit(subreddit: &str) -> Result<Vec<Post>> {
    let pushshift_url = Url::parse_with_params(
        format!("https://api.reddit.com/r/{}/new.json", subreddit).as_str(),
        &[("limit", NUM_POSTS.to_string().as_str())],
    )?;

    let resp = R_CLIENT.get(pushshift_url).send().await?;
    let body = resp.text().await?;

    let json: RedditResp = serde_json::from_str(&body)?;

    let temp = json.data.children.into_iter().map(|p| p.data).collect();
    Ok(temp)
}

async fn pushover(config: &CONFIG, posts: Vec<Post>) -> Result<()> {
    const POST_TIMEOUT_RETRY: i32 = 3;

    for post in posts.iter().rev() {
        // check if post id has been seen
        if DB.kv_contains(&post.id[..]) {
            continue;
        }

        println!("Found new post: {}", post.id);

        for attempt in 0..POST_TIMEOUT_RETRY {
            // set parameters
            let timestamp = match &post.created_utc {
                StringOrF64::String(s) => s.clone(),
                StringOrF64::F64(f) => format!("{}", f),
            };
            let decoded_title = htmlescape::decode_html(&post.title).unwrap_or(post.title.clone());
            let params = [
                ("token", &config.pushover_token),
                ("user", &config.pushover_user),
                ("title", &format!("New post on r/{}", "dreamcatcher")),
                ("message", &decoded_title),
                ("url", &format!("https://redd.it/{}", post.id)),
                ("timestamp", &timestamp),
            ];

            // send POST
            let resp = CLIENT
                .post("https://api.pushover.net/1/messages.json")
                .form(&params)
                .send()
                .await;

            // check if POST is ok
            let resp_ok = match resp {
                Ok(ok) => ok,
                Err(err) => {
                    if err.is_timeout() {
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
                        eprintln!("{:?}", err);
                        break;
                    }
                }
            };

            // add post id to database if success
            if resp_ok.status().is_success() {
                match resp_ok.text().await {
                    Err(err) => eprintln!("{:#?}", err), // could not parse resp body
                    Ok(ok) => {
                        let parsed_resp = serde_json::from_str::<POResp>(&ok[..]);
                        match parsed_resp {
                            Err(err) => eprintln!("{:#?}", err), // could not parse json
                            Ok(ok) => {
                                if ok.status == 1 {
                                    // store post id in database
                                    DB.kv_store(&post.id[..], "1")?;
                                    DB.commit()?;
                                    println!("Successfully pushed {}", post.id);
                                }
                            }
                        };
                    }
                };
            }

            break;
        }
    }

    Ok(())
}
