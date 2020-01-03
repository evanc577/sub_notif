extern crate serde;
extern crate serde_json;
extern crate unqlite;

use reqwest::Url;
use serde::Deserialize;
use std::fs;
use unqlite::{Transaction, UnQLite, KV};

const SEEN_DB: &str = "seen.udb";
const CONFIG_FILE: &str = "config.yaml";
const REQ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const NUM_POSTS: usize = 50;

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

fn main() {
    // parse config file
    let config = &parse_config();

    // open database of seen ids
    let unqlite = &UnQLite::create(&SEEN_DB);

    let subreddit = &config.subreddit;

    loop {
        // get recent posts from reddit
        let mut posts = match reddit(subreddit) {
            Ok(ok) => ok,
            Err(err) => {
                println!("{:#?}", err);
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
        };

        // get recent posts from pushshift
        let mut po_posts = match pushshift(subreddit) {
            Ok(ok) => ok,
            Err(err) => {
                println!("{:#?}", err);
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
        };

        // append pushover posts
        posts.append(&mut po_posts);

        // send notifications via pushover
        pushover(config, posts, unqlite);

        // sleep for a while
        std::thread::sleep(std::time::Duration::from_secs(10));
    }
}

fn parse_config() -> CONFIG {
    let contents = match fs::read_to_string(&CONFIG_FILE) {
        Ok(ok) => ok,
        Err(_) => {
            println!("Error: failed to open file {}", &CONFIG_FILE);
            panic!();
        }
    };

    match serde_yaml::from_str(&contents) {
        Ok(yaml) => yaml,
        Err(err) => {
            println!("Error parsing {}: {:?}", &CONFIG_FILE, err);
            panic!();
        }
    }
}

#[tokio::main]
async fn pushshift(subreddit: &str) -> Result<Vec<Post>, Box<dyn std::error::Error>> {
    let pushshift_url = Url::parse_with_params(
        "https://api.pushshift.io/reddit/submission/search",
        &[
            ("subreddit", &subreddit[..]),
            ("size", &NUM_POSTS.to_string()[..]),
        ],
    )?;

    let client = reqwest::Client::builder()
        .timeout(REQ_TIMEOUT)
        .build()?;
    let resp = client.get(pushshift_url).send().await?;

    let json: PSResp = serde_json::from_str(&resp.text().await?[..])?;

    Ok(json.data)
}

#[tokio::main]
async fn reddit(subreddit: &str) -> Result<Vec<Post>, Box<dyn std::error::Error>> {
    let pushshift_url = Url::parse_with_params(
        &format!("https://api.reddit.com/r/{}/new.json", subreddit)[..],
        &[("limit", &NUM_POSTS.to_string()[..])],
    )?;

    let client = reqwest::Client::builder()
        .timeout(REQ_TIMEOUT)
        .user_agent("sub notifier (by u/test241894)")
        .build()?;
    let resp = client.get(pushshift_url).send().await?;
    let body = &resp.text().await?[..];

    let json: RedditResp = serde_json::from_str(body)?;

    let mut temp = Vec::with_capacity(NUM_POSTS);
    for post in json.data.children {
        temp.push(post.data)
    }
    Ok(temp)
}

#[tokio::main]
async fn pushover(config: &CONFIG, posts: Vec<Post>, db: &UnQLite) {
    const POST_TIMEOUT_RETRY: i32 = 3;

    for post in posts.iter().rev() {
        // check if post id has been seen
        if db.kv_contains(&post.id[..]) {
            continue;
        }

        println!("Found new post: {}", post.id);

        for attempt in 0..POST_TIMEOUT_RETRY {
            // set parameters
            let timestamp = match &post.created_utc {
                StringOrF64::String(s) => s.clone(),
                StringOrF64::F64(f) => format!("{}", f),
            };
            let params = [
                ("token", &config.pushover_token),
                ("user", &config.pushover_user),
                ("title", &format!("New post on r/{}", "dreamcatcher")),
                ("message", &post.title),
                ("url", &format!("https://redd.it/{}", post.id)),
                ("timestamp", &timestamp),
            ];

            // send POST
            let client = reqwest::Client::builder()
                .timeout(REQ_TIMEOUT)
                .build()
                .unwrap();
            let resp = client
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
                        println!(
                            "POST {} to pushover timed out (attempt {} of {})",
                            post.id,
                            attempt + 1,
                            POST_TIMEOUT_RETRY
                        );
                        continue;
                    } else {
                        // break if failed for other reason
                        println!("{:?}", err);
                        break;
                    }
                }
            };

            // add post id to database if success
            if resp_ok.status().is_success() {
                match resp_ok.text().await {
                    Err(err) => println!("{:#?}", err), // could not parse resp body
                    Ok(ok) => {
                        let parsed_resp: Result<POResp, serde_json::Error> =
                            serde_json::from_str(&ok[..]);
                        match parsed_resp {
                            Err(err) => println!("{:#?}", err), // could not parse json
                            Ok(ok) => {
                                if ok.status == 1 {
                                    // store post id in database
                                    db.kv_store(&post.id[..], "1").unwrap();
                                    db.commit().unwrap();
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
}
