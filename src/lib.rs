use serde_json::json;
use discord_flows::{
    model::Message,
    ProvidedBot, Bot,
    message_handler,
};
use openai_flows::{
    embeddings::{EmbeddingsInput},
    chat::{ChatModel, ChatOptions, ChatRole, chat_history},
    OpenAIFlows,
};
use store_flows::{get, set};
use vector_store_flows::*;
use flowsnet_platform_sdk::logger;

// static SOFT_CHAR_LIMIT : usize = 20000; // GPT4 8k
static SOFT_CHAR_LIMIT : usize = 30000; // GPT35 16k

#[derive(Debug)]
struct ContentSettings {
    system_prompt: String,
    error_mesg: String,
    collection_name: String,
}

#[no_mangle]
#[tokio::main(flavor = "current_thread")]
pub async fn on_deploy() {
    let token = std::env::var("discord_token").unwrap();
    let bot = ProvidedBot::new(token);
    bot.listen_to_messages().await;
}

#[message_handler]
async fn handler(msg: Message) {
    logger::init();
    let discord_token = std::env::var("discord_token").unwrap();
    let bot = ProvidedBot::new(discord_token);

    let bot_id = std::env::var("bot_id").unwrap().parse::<u64>().unwrap();
    let cs = &ContentSettings {
        system_prompt: std::env::var("system_prompt").unwrap_or("".to_string()),
        error_mesg: std::env::var("error_mesg").unwrap_or("".to_string()),
        collection_name: std::env::var("collection_name").unwrap_or("".to_string()),
    };
    log::info!("The system prompt is {} lines", cs.system_prompt.lines().count());

    let discord = bot.get_client();
    if msg.author.bot {
        log::debug!("ignored bot message");
        return;
    }
    if msg.member.is_some() {
        let mut mentions_me = false;
        for u in &msg.mentions {
            log::debug!("The user ID is {}", u.id.as_u64());
            if *u.id.as_u64() == bot_id {
                mentions_me = true;
                break;
            }
        }
        if !mentions_me {
            log::debug!("ignored guild message");
            return;
        }
    }

    let channel_id = msg.channel_id;
    log::info!("Received message from {}", channel_id);

    let mut text = String::from(&msg.content);
    if text.eq_ignore_ascii_case("/new") {
        _ = discord.send_message(
            channel_id.into(),
            &serde_json::json!({
                "content": "Ok, I am starting a new conversation."
            }),
        ).await;
        set(&channel_id.to_string(), json!(true), None);
        log::info!("Restarted converstion for {}", channel_id);
        return;
    }

    let placeholder  = discord.send_message(
        channel_id.into(),
        &serde_json::json!({
            "content": "Typing ..."
        }),
    ).await.unwrap();

    let mut openai = OpenAIFlows::new();
    openai.set_retry_times(3);
                
    let restart = match get(&channel_id.to_string()) {
        Some(v) => v.as_bool().unwrap_or_default(),
        None => false,
    };

    let mut question_history = String::new();
    if !restart {
        match chat_history(&channel_id.to_string(), 8) {
            Some(v) => {
                for m in v.into_iter() {
                    if let ChatRole::User = m.role {
                        question_history.push_str(&m.content);
                        question_history.push_str("\n");
                    }
                }
            },
            None => (),
        };
    }
    question_history.push_str(&text);
    log::debug!("The question history is {}", question_history);

    // Compute embedding for the question
    let question_vector = match openai.create_embeddings(EmbeddingsInput::String(question_history)).await {
        Ok(r) => {
            if r.len() < 1 {
                log::error!("OpenAI returned no embedding for the question");
                _ = discord.edit_message(
                    channel_id.into(), placeholder.id.into(),
                    &serde_json::json!({
                        "content": &cs.error_mesg
                    }),
                ).await;
                return;
            }
            r[0].iter().map(|n| *n as f32).collect()
        }
        Err(e) => {
            log::error!("OpenAI returned an error: {}", e);
            _ = discord.edit_message(
                channel_id.into(), placeholder.id.into(),
                &serde_json::json!({
                    "content": &cs.error_mesg
                }),
            ).await;
            return;
        }
    };

    // Search for embeddings from the question
    let p = PointsSearchParams {
        vector: question_vector,
        limit: 5,
    };
    let mut system_prompt_updated = String::from(&cs.system_prompt);
    match search_points(&cs.collection_name, &p).await {
        Ok(sp) => {
            for p in sp.iter() {
                if system_prompt_updated.len() > SOFT_CHAR_LIMIT { break; }
                log::debug!("Received vector score={} and text={}", p.score, first_x_chars(p.payload.as_ref().unwrap().get("text").unwrap().as_str().unwrap(), 256));
                if p.score > 0.75 {
                    system_prompt_updated.push_str("\n");
                    system_prompt_updated.push_str(p.payload.as_ref().unwrap().get("text").unwrap().as_str().unwrap());
                }
            }
        }
        Err(e) => {
            log::error!("Vector search returns error: {}", e);
            _ = discord.edit_message(
                channel_id.into(), placeholder.id.into(),
                &serde_json::json!({
                    "content": &cs.error_mesg
                }),
            ).await;
            return;
        }
    }
    // log::debug!("The prompt is {} chars starting with {}", system_prompt_updated.len(), first_x_chars(&system_prompt_updated, 256));
    
    match system_prompt_updated.eq(&cs.system_prompt) {
        true =>  {
            log::info!("No relevant context for question");
            _ = discord.edit_message(
                channel_id.into(), placeholder.id.into(),
                &serde_json::json!({
                    "content": &cs.error_mesg
                }),
            ).await;
            return;
        },
        _ => (),
    }

    let co = ChatOptions {
        // model: ChatModel::GPT4,
        model: ChatModel::GPT35Turbo16K,
        restart: restart,
        system_prompt: Some(&system_prompt_updated),
        ..Default::default()
    };

    match openai.chat_completion(&channel_id.to_string(), &text, &co).await {
        Ok(r) => {
            let resps = sub_strings(&r.choice, 1800);

            _ = discord.edit_message(
                channel_id.into(), placeholder.id.into(),
                &serde_json::json!({
                    "content": resps[0]
                }),
            ).await;

            if resps.len() > 1 {
                for resp in resps.iter().skip(1) {
                    _  = discord.send_message(
                        channel_id.into(),
                        &serde_json::json!({
                            "content": resp
                        }),
                    ).await;
                }
            }
        }
        Err(e) => {
            _ = discord.edit_message(
                channel_id.into(), placeholder.id.into(),
                &serde_json::json!({
                    "content": &cs.error_mesg
                }),
            ).await;
            log::error!("OpenAI returns error: {}", e);
            return;
        }
    }

    // A successful restart. The new message will NOT be a restart
    if restart {
        log::info!("Detected restart = true");
        set(&channel_id.to_string(), json!(false), None);
    }
}

fn first_x_chars(s: &str, x: usize) -> String {
    s.chars().take(x).collect()
}

fn sub_strings(string: &str, sub_len: usize) -> Vec<&str> {
    let mut subs = Vec::with_capacity(string.len() / sub_len);
    let mut iter = string.chars();
    let mut pos = 0;

    while pos < string.len() {
        let mut len = 0;
        for ch in iter.by_ref().take(sub_len) {
            len += ch.len_utf8();
        }
        subs.push(&string[pos..pos + len]);
        pos += len;
    }
    subs
}
