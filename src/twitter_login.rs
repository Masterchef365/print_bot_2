use std::path::Path;
use anyhow::{Result, Context};
use egg_mode::{Token, KeyPair};

pub struct Config {
    pub user_id: u64,
    pub screen_name: String,
    access_token: KeyPair,
}

pub async fn login(con_token: KeyPair, persist_path: impl AsRef<Path>) -> Result<(Config, Token)> {
    if persist_path.as_ref().exists() {
        match try_login(con_token.clone(), &persist_path).await? {
            None => create_new_login(con_token, persist_path).await,
            Some(ct) => Ok(ct),
        }
    } else {
        create_new_login(con_token, persist_path).await
    }
}

async fn try_login(con_token: KeyPair, persist_path: impl AsRef<Path>) -> Result<Option<(Config, Token)>> {
    let config = Config::load(&persist_path).context("Failed to load config")?;

    let token = egg_mode::Token::Access {
        consumer: con_token,
        access: config.access_token.clone(),
    };

    if let Err(err) = egg_mode::auth::verify_tokens(&token).await {
        eprintln!("Warning; login from {:?}: {:?}", persist_path.as_ref(), err);
        std::fs::remove_file(persist_path)?;
        return Ok(None)
    } else {
        return Ok(Some((config, token)));
    }
}

async fn create_new_login(con_token: KeyPair, persist_path: impl AsRef<Path>) -> Result<(Config, Token)> {
    let request_token = egg_mode::auth::request_token(&con_token, "oob").await?;
    println!("Please sign in at {}", egg_mode::auth::authorize_url(&request_token));

    let pin = input_pin();
    let (token, user_id, screen_name) = egg_mode::auth::access_token(con_token, &request_token, pin.to_string()).await?;

    let access_token = match &token {
        Token::Access { access, .. } => access.clone(),
        _ => unreachable!(),
    };

    let config = Config {
        user_id,
        screen_name,
        access_token,
    };

    config.save(persist_path)?;

    Ok((config, token))
}

impl Config {
    fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;

        let mut lines = text.lines();
        let missing_line = "Parse error; missing line";

        let user_id = lines.next().context(missing_line)?.parse().context("User id is not an integer")?;
        let screen_name = lines.next().context(missing_line)?.to_string();

        let access_token = KeyPair::new(
            lines.next().context(missing_line)?.to_string(),
            lines.next().context(missing_line)?.to_string()
        );

        Ok(Self {
            user_id,
            screen_name,
            access_token,
        })
    }

    fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let mut text = String::new();
        text.push_str(&self.user_id.to_string());
        text.push('\n');
        text.push_str(&self.screen_name);
        text.push('\n');
        text.push_str(&self.access_token.key);
        text.push('\n');
        text.push_str(&self.access_token.secret);
        text.push('\n');
        Ok(std::fs::write(path, &text)?)
    }
}

fn input_pin() -> u32 {
    loop {
        println!("Please enter an integer PIN: ");
        let mut input_text = String::new();
        std::io::stdin()
            .read_line(&mut input_text)
            .expect("failed to read from stdin");

        let trimmed = input_text.trim();
        if let Ok(pin) = trimmed.parse::<u32>() {
            break pin;
        }
    }
}
