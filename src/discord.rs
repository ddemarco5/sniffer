// For sniffer post struct
use crate::reddit::SnifferPost;
use crate::Secrets;

use std::sync::Arc;
use tokio::select;
use tokio::sync::{RwLock, Mutex};
use tokio_util::sync::CancellationToken;

// For Discord
use serenity::{
    model::{id::ChannelId},
    client::{Client, bridge::gateway::ShardManager},
    async_trait,
    prelude::*,
    model::{event::ResumedEvent, gateway::{Ready, Activity}}
};


pub struct DiscordBot {
    serenity_bot: Arc<RwLock<Client>>,
    bot_http: Arc<serenity::http::client::Http>,
    shard_handle: Option<futures_locks::Mutex<tokio::task::JoinHandle<()>>>,
    shard_cancel_token: CancellationToken,
    shard_manager: Arc<Mutex<ShardManager>>,
    chat_channel: ChannelId,
    test_channel: ChannelId,
    archive_channel: ChannelId,
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        warn!("Connected as {}, setting bot to online", ready.user.name);
        set_status(&ctx).await;
    }

    async fn resume(&self, ctx: Context, _: ResumedEvent) {
        warn!("Resumed (reconnected)");
        set_status(&ctx).await;
    }
}

// The reset presence and activity action for both ready and result
async fn set_status(ctx: &Context) {
    ctx.reset_presence().await;
    ctx.set_activity(Activity::watching("the sniffer")).await;
}

impl DiscordBot {
    //pub async fn new(token: String, chat_channel: u64, archive_channel: u64, test_channel: u64) -> DiscordBot {
    pub async fn new(secrets: Secrets) -> DiscordBot {
        info!("Created the discord bot");
        // Configure the client with your Discord bot token in the environment.
        let token = secrets.bot_token;

        // Create a new instance of the Client, logging in as a bot. This will
        // automatically prepend your bot token with "Bot ", which is a requirement
        // by Discord for bot users.
        let serenity_bot = Client::builder(&token)
            .event_handler(Handler)
            .await
            .expect("Error creating client");
        // Get a shared ref of our http cache so we can use it to send messages in an async fashion
        let http = serenity_bot.cache_and_http.http.clone();
        // And for shard manager too
        let manager_clone = serenity_bot.shard_manager.clone();
        let bot = DiscordBot {
                serenity_bot: Arc::new(RwLock::new(serenity_bot)),
                bot_http: http,
                shard_handle: None,
                shard_cancel_token: CancellationToken::new(),
                shard_manager: manager_clone,
                chat_channel: ChannelId(secrets.main_channel), // main channel
                test_channel: ChannelId(secrets.test_channel),
                archive_channel: ChannelId(secrets.archive_channel), // the archive channel
            };

        return bot;
    }

    pub async fn start_shards(&mut self, num_shards: u64) {
        let bot = self.serenity_bot.clone();
        let cloned_token = self.shard_cancel_token.clone();
        self.shard_handle = Some(futures_locks::Mutex::new(
            tokio::spawn(async move {
                let mut lock = bot.write().await;
                select! {
                    _ = lock.start_shards(num_shards) => {  
                        warn!("Shard threads stopped")
                    }
                    _ = cloned_token.cancelled() => {
                        warn!{"Cancelled our shards"}
                    }
                }
            })
        ));
        warn!("Started shards");
        
    }

    pub async fn print_shard_info(&self) {
        let lock = self.shard_manager.lock().await;
        let shard_runners = lock.runners.lock().await;
        for (id, runner) in shard_runners.iter() {
            warn!(
                "Shard ID {} is {} with a latency of {:?}",
                id, runner.stage, runner.latency,
            );
        }
    }

    pub async fn stop_shards(&mut self) {
        // Start the cancel
        self.shard_cancel_token.cancel();
        // Wait on our handle
        match &self.shard_handle{
            Some(x) => {
                let handle_lock = x.lock();
                handle_lock.await;
                warn!("Successfully waited on future");
                //handle_box.await.expect("failed waiting for the sharts to end");
                //*handle_lock.await;
            }
            None => {
                error!("We don't have a shard handle")
            }
        }
    }

    pub async fn post_message(&self, message: SnifferPost) {
        let http = &self.bot_http;
        info!("Trying to send message: {}", message);
        let mut message_text = message.discord_string();

        // Send message to our primary channel
        self.chat_channel.say(&http, message_text.clone()).await.expect("Error sending message to main channel");

        // Send message to our archive channel with url attached
        // Append the post url to this one if we have it
        match message.url { 
            Some(m) => {
                message_text.push_str(format!("\n<{}>", m).as_str());
            }
            None => {}
        }
        self.archive_channel.say(&http, message_text).await.expect("Error sending message to archive");
    }

    #[allow(dead_code)]
    pub async fn post_debug_string(&self, message: String) {
        let http = &self.bot_http;
        warn!("Trying to send debug message");
        self.test_channel.say(&http, message.clone()).await.expect("Error sending test message");
    }

}

impl Clone for DiscordBot {
    fn clone(&self) -> Self {
        DiscordBot {
            serenity_bot: self.serenity_bot.clone(),
            bot_http: self.bot_http.clone(),
            shard_handle: {
                match &self.shard_handle {
                    Some(h) => Some(h.clone()),
                    None => None,
                }
            },
            shard_cancel_token: self.shard_cancel_token.clone(),
            shard_manager: self.shard_manager.clone(),
            chat_channel: self.chat_channel.clone(),
            test_channel: self.test_channel.clone(),
            archive_channel: self.archive_channel.clone(),
        }
    }
}

