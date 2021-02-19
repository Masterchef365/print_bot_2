use anyhow::{Context, Result};
use discord::model::Event;
use discord::Discord;
use log::{error, info, LevelFilter};
use std::env;
use std::path::PathBuf;
use std::thread;
use structopt::StructOpt;

mod printer;
use printer::PrintHandler;
mod camera;
use camera::CameraHandler;

#[derive(Debug, StructOpt)]
#[structopt(name = "example", about = "An example of StructOpt usage.")]
struct Opt {
    /// Disable the camera
    #[structopt(long)]
    disable_camera: bool,

    /// Disable the printer
    #[structopt(long)]
    disable_printer: bool,

    /// Logging path
    #[structopt(long, default_value = "print_bot.log")]
    log_path: PathBuf,
}

// Settings
pub const HELP_COMMAND: &str = "!help";
pub const PRINT_COMMAND: &str = "!print";
pub const SHOW_COMMAND: &str = "!showme";

/// Log a result as an error
pub fn log_result(res: Result<()>) {
    if let Err(e) = res {
        error!("Error: {:#}", e);
    }
}

/// Log a fatal error and panic
pub fn fatal_error(res: Result<()>) {
    if res.is_err() {
        log_result(res);
        panic!("Fatal error");
    }
}

fn main() -> Result<()> {
    // Arg parsing
    let opt = Opt::from_args();

    // Set up logging
    simple_logging::log_to_file(opt.log_path, LevelFilter::Info)?;

    // Arguments
    let token = env::var("DISCORD_TOKEN")
        .context("Expected token in DISCORD_TOKEN environment variable.")?;

    // Set up printer concurrently with logging into Discord
    let print_handler = if opt.disable_printer {
        None
    } else {
        Some(thread::spawn(|| {
            PrintHandler::new().map_err(|e| e.to_string())
        }))
    };

    // Log in to Discord using a bot token from the environment
    info!("Logging into discord");
    let discord = Discord::from_bot_token(&token).context("login failed")?;

    let mut print_handler = print_handler.map(|p| p.join().unwrap().unwrap());

    // Establish and use a websocket connection
    let (mut connection, _) = discord.connect().context("connect failed")?;

    info!("Ready.");
    loop {
        match connection.recv_event() {
            Ok(Event::MessageCreate(message)) => {
                // No bots!
                if message.author.bot {
                    continue;
                }

                // Parse command from message
                let cmd = match message.content.split_whitespace().next() {
                    Some(cmd) => cmd,
                    None => continue,
                };

                // Run command
                match cmd {
                    PRINT_COMMAND => {
                        if let Some(handler) = &mut print_handler {
                            log_result(handler.handle_print_request(message));
                        } else {
                            discord.send_message(message.channel_id, SORRY_PRINTER, "", false)?;
                        }
                    }
                    HELP_COMMAND => {
                        discord.send_message(message.channel_id, HELP_TEXT, "", false)?;
                    }
                    SHOW_COMMAND => {
                    }
                    _ => (),
                }
            }
            Ok(_) => {}
            Err(discord::Error::Closed(code, body)) => {
                error!("Gateway closed on us with code {:?}: {}", code, body);
                break;
            }
            Err(err) => error!("Receive error: {:?}", err),
        }
    }
    Ok(())
}

const HELP_TEXT: &str = "
**Segfault's printer bot**\n
This bot uses a receipt printer to immortalize your messages on 58mm thermal paper. Printer paper is extra super cheap, but remember that whatever you do print is waste.
If this command works, the printer _should_ be running. Have fun!

__Commands__:
`!print`: Print text or an image URL following this command, or attached images.
`!help`: Print this message
`!show`: Take a picture of the printer, and show it here.
";

const SORRY_PRINTER: &str = "Sorry, the printer has been disabled for now :(";
