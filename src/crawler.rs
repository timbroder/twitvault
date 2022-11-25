use crate::storage::{List, Storage};
use crate::types::Message;
use egg_mode::{
    cursor,
    list::{self, ListID},
    tweet::{self, Tweet},
    user::{self, TwitterUser},
    RateLimit,
};
use reqwest::Client;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io::Write;
use std::{collections::HashSet, path::PathBuf, str::FromStr, sync::Arc};
use tokio::sync::{
    mpsc::{channel, Sender},
    Mutex,
};
use tracing::{info, trace, warn};

use eyre::Result;

use crate::config::Config;

/// Internal messaging between the different threads
#[derive(Debug)]
enum DownloadInstruction {
    Image(String),
    Movie(mime::Mime, String),
    ProfileMedia(String),
    Done,
}

pub async fn fetch(config: &Config, storage: Storage, sender: Sender<Message>) -> Result<()> {
    let user_id = storage.data().profile.id;
    let shared_storage = Arc::new(Mutex::new(storage));

    async fn msg(msg: impl AsRef<str>, sender: &Sender<Message>) -> Result<()> {
        Ok(sender
            .send(Message::Loading(msg.as_ref().to_string()))
            .await?)
    }

    async fn save_data(storage: &Arc<Mutex<Storage>>) {
        if let Err(e) = storage.lock().await.save() {
            warn!("Could not write out data {e:?}");
        }
    }

    let (instruction_sender, mut instruction_receiver) = channel(4096);
    let cloned_storage = shared_storage.clone();
    let download_media = config.crawl_options().media;
    let instruction_task = tokio::spawn(async move {
        let client = Client::new();
        while let Some(instruction) = instruction_receiver.recv().await {
            if matches!(instruction, DownloadInstruction::Done) {
                break;
            }
            if !download_media {
                continue;
            }
            if let Err(e) = handle_instruction(&client, instruction, cloned_storage.clone()).await {
                warn!("Download Error {e:?}");
            }
        }
    });

    if config.crawl_options().tweets {
        msg("User Tweets", &sender).await?;
        fetch_user_tweets(
            user_id,
            shared_storage.clone(),
            config,
            instruction_sender.clone(),
        )
        .await?;
        save_data(&shared_storage).await;
    }

    if config.crawl_options().mentions {
        msg("User Mentions", &sender).await?;
        fetch_user_mentions(shared_storage.clone(), config, instruction_sender.clone()).await?;
        save_data(&shared_storage).await;
    }

    if config.crawl_options().followers {
        msg("Followers", &sender).await?;
        fetch_user_followers(
            user_id,
            shared_storage.clone(),
            config,
            instruction_sender.clone(),
        )
        .await?;
        save_data(&shared_storage).await;
    }

    if config.crawl_options().follows {
        msg("Follows", &sender).await?;
        fetch_user_follows(
            user_id,
            shared_storage.clone(),
            config,
            instruction_sender.clone(),
        )
        .await?;
        save_data(&shared_storage).await;
    }

    if config.crawl_options().lists {
        msg("Lists", &sender).await?;
        fetch_lists(
            user_id,
            shared_storage.clone(),
            config,
            instruction_sender.clone(),
        )
        .await?;
        save_data(&shared_storage).await;
    }

    instruction_sender.send(DownloadInstruction::Done).await?;
    instruction_task.await?;

    let storage = shared_storage.lock_owned().await.clone();
    sender.send(Message::Finished(storage)).await?;

    Ok(())
}

async fn fetch_user_tweets(
    id: u64,
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    let mut timeline = tweet::user_timeline(id, true, true, &config.token).with_page_size(50);

    let mut first_page = config.paging_position("user_tweets");

    loop {
        tracing::info!("Downloading Tweets before {:?}", timeline.min_id);
        let (next_timeline, mut feed) = timeline.older(first_page).await?;
        first_page = None;
        if feed.response.is_empty() {
            break;
        }
        for tweet in feed.response.iter() {
            inspect_tweet(tweet, shared_storage.clone(), config, &sender).await?;
        }
        shared_storage
            .lock()
            .await
            .data_mut()
            .tweets
            .append(&mut feed.response);

        handle_rate_limit(&feed.rate_limit_status, "User Feed").await;
        timeline = next_timeline;
        config.set_paging_position("user_tweets", timeline.min_id)
    }

    config.set_paging_position("user_tweets", None);

    Ok(())
}

async fn fetch_user_mentions(
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    let mut timeline = tweet::mentions_timeline(&config.token).with_page_size(50);

    let mut first_page = config.paging_position("user_mentions");

    loop {
        tracing::info!("Downloading Mentions before {:?}", timeline.min_id);
        let (next_timeline, mut feed) = timeline.older(first_page).await?;
        first_page = None;
        if feed.response.is_empty() {
            break;
        }
        for tweet in feed.response.iter() {
            inspect_tweet(tweet, shared_storage.clone(), config, &sender).await?;
        }
        shared_storage
            .lock()
            .await
            .data_mut()
            .mentions
            .append(&mut feed.response);

        handle_rate_limit(&feed.rate_limit_status, "User Mentions").await;
        timeline = next_timeline;
        config.set_paging_position("user_mentions", timeline.min_id)
    }

    config.set_paging_position("user_mentions", None);

    Ok(())
}

async fn fetch_user_followers(
    id: u64,
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    let ids = fetch_profiles_ids(
        "followers",
        user::followers_ids(id, &config.token).with_page_size(50),
        shared_storage.clone(),
        config,
        sender,
    )
    .await?;
    shared_storage.lock().await.data_mut().followers = ids;
    Ok(())
}

async fn fetch_user_follows(
    id: u64,
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    let ids = fetch_profiles_ids(
        "follows",
        user::friends_ids(id, &config.token).with_page_size(50),
        shared_storage.clone(),
        config,
        sender,
    )
    .await?;
    shared_storage.lock().await.data_mut().follows = ids;
    Ok(())
}

// Helpers

async fn fetch_profiles_ids(
    kind: &'static str,
    mut cursor: cursor::CursorIter<cursor::IDCursor>,
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<Vec<u64>> {
    let mut ids = Vec::new();
    cursor.next_cursor = config.paging_position(kind).map(|e| e as i64).unwrap_or(-1);
    loop {
        info!("Downloading {kind} before {}", cursor.next_cursor);
        let called = cursor.call();
        let resp = match called.await {
            Ok(n) => n,
            Err(e) => {
                warn!("Profile Ids Error {e:?}");
                continue;
            }
        };

        let mut new_ids = resp.response.ids.clone();

        if new_ids.is_empty() {
            break;
        }

        fetch_multiple_profiles_data(&new_ids, shared_storage.clone(), config, sender.clone())
            .await?;

        ids.append(&mut new_ids);

        handle_rate_limit(&resp.rate_limit_status, kind).await;
        cursor.next_cursor = resp.response.next_cursor;
        config.set_paging_position(kind, u64::try_from(cursor.next_cursor).ok());
    }

    config.set_paging_position(kind, None);

    Ok(ids)
}

async fn fetch_multiple_profiles_data(
    ids: &[u64],
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    // only get profiles we haven't gotten yet
    let known_ids: HashSet<u64> = shared_storage
        .lock()
        .await
        .data()
        .profiles
        .keys()
        .copied()
        .collect();
    let filtered: Vec<_> = ids
        .iter()
        .filter(|id| !known_ids.contains(id))
        .copied()
        .collect();
    info!("Downloading {} profiles", filtered.len());
    let profiles = user::lookup(filtered, &config.token).await?;
    for profile in profiles.iter() {
        inspect_profile(profile, sender.clone()).await?;
    }
    shared_storage.lock().await.with_data(move |data| {
        for profile in &profiles.response {
            data.profiles.insert(profile.id, profile.clone());
        }
    });
    Ok(())
}

async fn fetch_lists(
    id: u64,
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    let mut cursor = list::ownerships(id, &config.token).with_page_size(500);
    cursor.next_cursor = config
        .paging_position("lists")
        .map(|e| e as i64)
        .unwrap_or(-1);
    loop {
        let called = cursor.call();
        let resp = match called.await {
            Ok(n) => n,
            Err(e) => {
                warn!("Lists Error {e:?}");
                continue;
            }
        };

        let lists = resp.response.lists;

        if lists.is_empty() {
            break;
        }

        for list in lists {
            info!("Fetching members for list {}", list.full_name);
            fetch_list_members(list, shared_storage.clone(), config, sender.clone()).await?;
        }

        handle_rate_limit(&resp.rate_limit_status, "Lists").await;
        cursor.next_cursor = resp.response.next_cursor;
        config.set_paging_position("lists", u64::try_from(cursor.next_cursor).ok());
    }

    config.set_paging_position("lists", None);
    Ok(())
}

async fn fetch_list_members(
    list: list::List,
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    let list_id = ListID::from_id(list.id);
    let mut cursor = list::members(list_id, &config.token).with_page_size(2000);
    let paging_key = format!("list-{}", list.id);
    cursor.next_cursor = config
        .paging_position(&paging_key)
        .map(|e| e as i64)
        .unwrap_or(-1);
    let mut member_ids = Vec::new();
    loop {
        let called = cursor.call();
        let resp = match called.await {
            Ok(n) => n,
            Err(e) => {
                warn!("List Members Error {e:?}");
                continue;
            }
        };

        if resp.users.is_empty() {
            break;
        }

        let mut storage = shared_storage.lock().await;

        info!("Processing {} members", resp.users.len());
        for member in &resp.users {
            if let Err(e) = inspect_profile(member, sender.clone()).await {
                warn!("Could not inspect profile {e:?}");
            }
            member_ids.push(member.id);
            storage
                .data_mut()
                .profiles
                .insert(member.id, member.clone());
        }

        handle_rate_limit(&resp.rate_limit_status, "List Members").await;
        cursor.next_cursor = resp.response.next_cursor;
        config.set_paging_position(&paging_key, u64::try_from(cursor.next_cursor).ok());
    }

    config.set_paging_position(&paging_key, None);

    shared_storage.lock().await.data_mut().lists.push(List {
        name: list.name.clone(),
        list,
        members: member_ids,
    });

    Ok(())
}

async fn fetch_single_profile(
    id: u64,
    shared_storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    if shared_storage
        .lock()
        .await
        .data()
        .profiles
        .contains_key(&id)
    {
        return Ok(());
    }

    let user = user::show(id, &config.token).await?;
    if let Err(e) = inspect_profile(&user, sender).await {
        warn!("Inspect profile error {e:?}");
    }

    shared_storage
        .lock()
        .await
        .data_mut()
        .profiles
        .insert(id, user.response);
    Ok(())
}

async fn inspect_tweet(
    tweet: &Tweet,
    storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: &Sender<DownloadInstruction>,
) -> Result<()> {
    if let Err(e) = inspect_inner_tweet(tweet, config, &storage, sender.clone()).await {
        warn!("Inspect Tweet Error {e:?}");
    }

    if let Some(quoted_tweet) = &tweet.quoted_status {
        if let Err(e) = inspect_inner_tweet(quoted_tweet, config, &storage, sender.clone()).await {
            warn!("Inspect Quoted Tweet Error {e:?}");
        }
    }

    if let Some(retweet) = &tweet.retweeted_status {
        if let Err(e) = inspect_inner_tweet(retweet, config, &storage, sender.clone()).await {
            warn!("Inspect Retweet Error {e:?}");
        }
    }

    if config.crawl_options().tweet_responses {
        // for our own tweets, we search for responses
        if tweet.user.is_none() || tweet.user.as_ref().map(|e| e.id) == Some(config.user_id()) {
            if let Err(e) = fetch_tweet_replies(tweet, storage.clone(), config, sender).await {
                warn!("Could not fetch replies for tweet {}: {e:?}", tweet.id);
            }
        }
    }

    Ok(())
}

async fn inspect_inner_tweet(
    tweet: &Tweet,
    config: &Config,
    storage: &Arc<Mutex<Storage>>,
    sender: Sender<DownloadInstruction>,
) -> Result<()> {
    if config.crawl_options().tweet_profiles {
        if let Some(user) = &tweet.user {
            if user.id != config.user_id() {
                if let Err(e) =
                    fetch_single_profile(user.id, storage.clone(), config, sender.clone()).await
                {
                    warn!("Could not download profile {e:?}");
                }
            }
        }
    }

    let Some(entities) = &tweet.extended_entities else { return Ok(()) };

    for media in &entities.media {
        match &media.video_info {
            Some(n) => {
                let mut selected_variant = n.variants.first();
                for variant in &n.variants {
                    match (
                        variant.content_type.subtype(),
                        &selected_variant.map(|e| e.bitrate),
                    ) {
                        (mime::MP4, Some(bitrate)) if bitrate > &variant.bitrate => {
                            selected_variant = Some(variant)
                        }
                        _ => (),
                    }
                }
                let Some(variant) = selected_variant else { continue };
                if let Err(e) = sender
                    .send(DownloadInstruction::Movie(
                        variant.content_type.clone(),
                        variant.url.clone(),
                    ))
                    .await
                {
                    warn!("Send Error {e:?}");
                }
            }
            None => {
                if let Err(e) = sender
                    .send(DownloadInstruction::Image(media.media_url_https.clone()))
                    .await
                {
                    warn!("Send Error {e:?}");
                }
            }
        }
    }
    Ok(())
}

async fn fetch_tweet_replies(
    tweet: &Tweet,
    storage: Arc<Mutex<Storage>>,
    config: &Config,
    sender: &Sender<DownloadInstruction>,
) -> Result<()> {
    let search_results = egg_mode::search::search(format!("to:{}", config.screen_name()))
        .since_tweet(tweet.id)
        .count(50)
        .call(&config.token)
        .await?;
    handle_rate_limit(&search_results.rate_limit_status, "Tweet Replies").await;

    let mut replies = Vec::new();

    for related_tweet in search_results.response.statuses.into_iter() {
        if related_tweet.in_reply_to_status_id == Some(tweet.id) {
            if let Err(e) =
                inspect_inner_tweet(&related_tweet, config, &storage, sender.clone()).await
            {
                warn!("Could not inspect tweet {}: {e:?}", related_tweet.id);
            }
            replies.push(related_tweet);
        }
    }

    let mut shared_storage = storage.lock().await;
    shared_storage
        .data_mut()
        .responses
        .insert(tweet.id, replies);

    Ok(())
}

async fn inspect_profile(profile: &TwitterUser, sender: Sender<DownloadInstruction>) -> Result<()> {
    if let Some(background_image) = profile.profile_background_image_url_https.as_ref() {
        sender
            .send(DownloadInstruction::ProfileMedia(background_image.clone()))
            .await?;
    }
    if let Some(profile_banner_url) = profile.profile_banner_url.as_ref() {
        sender
            .send(DownloadInstruction::ProfileMedia(
                profile_banner_url.clone(),
            ))
            .await?;
    }
    sender
        .send(DownloadInstruction::ProfileMedia(
            profile.profile_image_url_https.clone(),
        ))
        .await?;
    Ok(())
}

async fn handle_instruction(
    client: &Client,
    instruction: DownloadInstruction,
    shared_storage: Arc<Mutex<Storage>>,
) -> Result<()> {
    let (extension, url) = match instruction {
        DownloadInstruction::Image(url) => (extension_for_url(&url), url),
        DownloadInstruction::Movie(mime, url) => (
            match mime.subtype().as_str().to_lowercase().as_str() {
                "mp4" => "mp4".to_string(),
                "avi" => "avi".to_string(),
                "3gp" => "3gp".to_string(),
                "mov" => "mov".to_string(),
                _ => extension_for_url(&url),
            },
            url,
        ),
        DownloadInstruction::ProfileMedia(url) => (extension_for_url(&url), url),
        _ => return Ok(()),
    };
    let path = {
        let storage = shared_storage.lock().await;
        if storage.data().media.contains_key(&url) {
            return Ok(());
        }
        let mut hasher = DefaultHasher::new();
        hasher.write(url.as_bytes());
        let file_name = format!("{}.{extension}", hasher.finish());
        storage.media_path(&file_name)
    };

    let mut fp = std::fs::File::create(&path)?;

    let bytes = client.get(&url).send().await?.bytes().await?;

    fp.write_all(&bytes)?;

    shared_storage
        .lock()
        .await
        .data_mut()
        .media
        .insert(url, path);

    Ok(())
}

fn extension_for_url(url: &str) -> String {
    let default = "png".to_string();
    let Ok(parsed) = url::Url::parse(url) else {
        return default;
    };
    let Some(Some(last_part)) = parsed.path_segments().and_then(|e| e.last().map(|p| PathBuf::from_str(p).ok())) else {
        return default;
    };
    let Some(extension) = last_part.extension().and_then(|e| e.to_str().map(|s| s.to_string())) else {
        return default
    };
    extension
}

/// If the rate limit for a call is used up, delay that particular call
async fn handle_rate_limit(limit: &RateLimit, call_info: &'static str) {
    if limit.remaining <= 1 {
        let seconds = {
            use std::time::{SystemTime, UNIX_EPOCH};
            match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(n) => (((limit.reset as i64) - n.as_secs() as i64) + 10) as u64,
                Err(_) => 1000,
            }
        };
        info!("Rate limit for {call_info} reached. Waiting {seconds} seconds");
        let wait_duration = tokio::time::Duration::from_secs(seconds);
        tokio::time::sleep(wait_duration).await;
    } else {
        trace!(
            "Rate limit for {call_info}: {} / {}",
            limit.remaining,
            limit.limit
        );
    }
}