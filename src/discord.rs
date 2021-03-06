// For sniffer post struct
use crate::reddit::SnifferPost;
use crate::Secrets;
use crate::audio::player::{AudioPlayer};
use crate::commands::Parser;

use std::sync::Arc;
use tokio::select;
use tokio::sync::{RwLock, Mutex};
use tokio_util::sync::CancellationToken;

// For Discord
use serenity::{
    prelude::*,
    model::{id::{ChannelId, EmojiId}},
    model::{event::ResumedEvent, gateway::{Ready, Activity}},
    client::{Client, bridge::gateway::ShardManager},
    model::channel::{Message, ReactionType},
    async_trait,
};

// Enable songbird register trait for serenity
use songbird::SerenityInit;

// The reset presence and activity action for both ready and result
async fn set_status(ctx: &Context) {
    ctx.reset_presence().await;
    ctx.set_activity(Activity::watching("the sniffer")).await;
}

fn react_success(ctx: &Context, message: &Message) {
    tokio::task::block_in_place(move || {
        tokio::runtime::Handle::current().block_on(async move {
            message.react(ctx.http.clone(), ReactionType::Custom{
                animated: false,
                id: EmojiId(801166698610294895),
                name: Some(String::from(":guthchamp:")),
            }).await.expect("Failed to react to post");
        })
    });
}

fn react_fail(ctx: &Context, message: &Message) {
    tokio::task::block_in_place(move || {
        tokio::runtime::Handle::current().block_on(async move {
            message.react(ctx.http.clone(), ReactionType::Custom{
                animated: false,
                id: EmojiId(886356280934006844),
                name: Some(String::from(":final_pepe:")),
            }).await.expect("Failed to react to post");
        })
    });
}

struct BotEventHandler {
    listen_channel: ChannelId,
    parser: Parser,
}

#[async_trait]
impl EventHandler for BotEventHandler {

    async fn ready(&self, ctx: Context, ready: Ready) {
        warn!("Connected as {}, setting bot to online", ready.user.name);
        set_status(&ctx).await;
    }

    async fn resume(&self, ctx: Context, _: ResumedEvent) {
        warn!("Resumed (reconnected)");
        set_status(&ctx).await;
    }

    async fn message(&self, ctx: Context, new_message: Message) {
        // Make sure we're listening in our designated channel, and we ignore messages from ourselves
        if (new_message.channel_id == self.listen_channel) && !new_message.author.bot {

            match self.parser.process(&ctx, &new_message).await {
                Ok(()) => {
                    react_success(&ctx, &new_message);
                }
                Err(e) => {
                    error!("{}", e);
                    react_fail(&ctx, &new_message);
                }
            }
        }
    }
}



pub struct DiscordBot {
    serenity_bot: Arc<RwLock<Client>>,
    bot_http: Arc<serenity::http::client::Http>,
    shard_handle: Option<futures_locks::Mutex<tokio::task::JoinHandle<()>>>,
    shard_cancel_token: CancellationToken,
    shard_manager: Arc<Mutex<ShardManager>>,
    chat_channel: ChannelId,
    test_channel: ChannelId,
    archive_channel: ChannelId,
    audio_player: Arc<Mutex<AudioPlayer>>,
    command_parser: Parser,
}

impl DiscordBot {
    pub async fn new(secrets: Secrets) -> DiscordBot {
        info!("Created the discord bot");
        // Configure the client with your Discord bot token in the environment.
        let token = secrets.bot_token;
        let audio_channel = ChannelId(secrets.audio_channel);

        // Create an audio player in a mutex and its serenity callback listener
        let audio_player_lock = AudioPlayer::new(
            secrets.audio_channel, 
            10,
            std::time::Duration::from_secs(60),
        ).await;
        warn!("Created audio player instance");

        // Create our command parser
        let parser = Parser::new(audio_player_lock.clone()); // Give it the lock as it'll need to run audio commands

        // Create a new instance of the Client, logging in as a bot. This will
        // automatically prepend your bot token with "Bot ", which is a requirement
        // by Discord for bot users.
        let mut audioplayer = audio_player_lock.lock().await; // Lock the player so we can do some work
        let serenity_bot = Client::builder(&token)
            .event_handler(BotEventHandler{ listen_channel: audio_channel, parser: parser.clone() })
            .register_songbird_with(audioplayer.get_songbird())
            .await
            .expect("Error creating client");
        // Initialize songbird with it
        audioplayer.init_player(serenity_bot.cache_and_http.clone(), 1, secrets.guild_id).await;
        drop(audioplayer); // drop the lock so we can pass it off to our bot struct

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
                audio_player: audio_player_lock.clone(),
                command_parser: parser,
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
                        warn!("Cancelled our shards")
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

    pub async fn shutdown(self) {
        self.stop_audio().await;
        self.stop_shards().await; // we hold a write lock on serenity here, it's its run future
    }

    //TODO: Find a way to make sure we can get the same instance of our original audio player
    async fn stop_audio(&self) {
        // If we have a player, hang up
        //if let Some(player_lock) = &self.audio_player {
            let mut player = self.audio_player.lock().await;
            if let Err(x) = player.shutdown() {
                error!("Error shutting down player: {}", x);
            }
            // This is dumb as hell, but if we don't wait a little bit we'll remove the shards
            // before it has a chance to leave, they should really have a leave_blocking function
            // There's nothing we can poll to check to see if we've fully left either, the
            // associated structs reflect the state immediately

            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        //}
    }

    async fn stop_shards(&self) {
        // Start the cancel
        self.shard_cancel_token.cancel();
        // Wait on our handle
        match &self.shard_handle{
            Some(x) => {
                let handle_lock = x.lock();
                handle_lock.await;
                warn!("Successfully waited on future");
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
            audio_player: self.audio_player.clone(),
            command_parser: self.command_parser.clone(),
        }
    }
}

