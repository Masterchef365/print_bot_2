use anyhow::{format_err, Context, Result};
use discord::model::Event;
use discord::Discord;
use log::{error, info, LevelFilter};
use std::env;
use std::path::PathBuf;
use std::thread;
use structopt::StructOpt;

use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::Device;
use v4l::FourCC;

mod printer;
use printer::PrintHandler;

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
        error!("{:#}", e);
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
        Some(thread::spawn(|| -> Result<PrintHandler> {
            Ok(PrintHandler::new()?)
        }))
    };

    // Log in to Discord using a bot token from the environment
    info!("Logging into discord");
    let discord = Discord::from_bot_token(&token).context("login failed")?;

    // Wait for the print handler...
    let mut print_handler = print_handler.and_then(|p| p.join().ok()).transpose()?;

    // Establish and use a websocket connection
    let (mut connection, _) = discord.connect().context("connect failed")?;

    // ################# CAMERA ######################

    // Create a new capture device with a few extra parameters
    let mut dev = (!opt.disable_camera)
        .then(|| -> Result<Device> {
            let dev = Device::new(0).context("Open device")?;

            // Let's say we want to explicitly request another format
            let mut fmt = dev.format().context("Read format")?;
            fmt.width = 1280;
            fmt.height = 720;
            fmt.fourcc = FourCC::new(b"MJPG");
            dev.set_format(&fmt).context("Write format")?;
            Ok(dev)
        })
        .transpose()
        .context("Failed to open camear")?;

    // Create the stream, which will internally 'allocate' (as in map) the
    // number of requested buffers for us.
    let mut stream = dev
        .as_mut()
        .map(|dev| -> Result<Stream> {
            let mut stream = Stream::with_buffers(dev, Type::VideoCapture, 4)
                .context("Failed to create buffer stream")?;

            // Prime the camera
            let steps = 5;
            for i in 1..=steps {
                info!("Priming the camera {}/{}", i, steps);
                stream.next()?;
            }

            Ok(stream)
        })
        .transpose()?;

    // ###############################################

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
                        info!(
                            "{}#{} began a print job.",
                            message.author.name, message.author.discriminator
                        );
                        if let Some(handler) = &mut print_handler {
                            log_result(handler.handle_print_request(message));
                        } else {
                            discord.send_message(message.channel_id, SORRY_PRINTER, "", false)?;
                        }
                    }
                    HELP_COMMAND => {
                        discord.send_message(message.channel_id, HELP_TEXT, "", false)?;
                    }
                    SHOW_COMMAND => match stream
                        .as_mut()
                        .ok_or_else(|| format_err!("Camera not set up"))
                        .and_then(|s| Ok(s.next()?))
                    {
                        Ok((buf, _)) => {
                            info!(
                                "{}#{} took a picture.",
                                message.author.name, message.author.discriminator
                            );
                            discord
                                .send_file(message.channel_id, "", buf, "image.jpg")
                                .context("Failed to send image file!")?;
                        }
                        Err(e) => {
                            error!("Camera error: {}", e);
                            discord.send_message(message.channel_id, SORRY_CAMERA, "", false)?;
                        }
                    },
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
`!showme`: Take a picture of the printer, and show it here.
";

const SORRY_PRINTER: &str = "Sorry, the printer has been disabled for now :(";
const SORRY_CAMERA: &str = "Sorry, the camera has been disabled for now :(";
