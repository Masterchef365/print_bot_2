use anyhow::{Context, Result};
use discord::model::Event;
use discord::{model::Message, Discord};
use dither::prelude::*;
use hyper::net::HttpsConnector;
use hyper::Client;
use hyper_native_tls::NativeTlsClient;
use image::GenericImageView;
use std::env;
use std::io::Read;
use std::str::FromStr;

struct Handler {
    client: Client,
    ditherer: Ditherer<'static>,
}

impl Handler {
    pub fn new() -> Result<Self> {
        let ssl = NativeTlsClient::new()?;
        let connector = HttpsConnector::new(ssl);
        let client = hyper::Client::with_connector(connector);
        let ditherer = Ditherer::from_str("floyd")?;
        Ok(Self { client, ditherer })
    }

    pub fn handle(&mut self, message: Message) -> Result<()> {
        if !message.content.starts_with("!print") {
            return Ok(());
        }

        for att in message.attachments {
            // Download the image
            let mut image = self
                .client
                .get(&att.url)
                .send()
                .context("Image download failed")?;

            // Read the image into local memory
            let mut buf = Vec::new();
            image.read_to_end(&mut buf).context("Image read failed")?;

            // Decode the image
            let image = image::load_from_memory(&buf).context("Image parse failed")?;

            // Convert to the ditherer's image format
            let image: Img<RGB<f64>> = Img::new(
                image.to_rgb8().pixels().map(|p| RGB::from(p.0)),
                image.width(),
            )
            .context("Image convert failed")?;

            // Dither the image
            let quantize = dither::create_quantize_n_bits_func(1)?;
            let image = image.convert_with(|rgb| rgb.to_chroma_corrected_black_and_white());
            let image = self
                .ditherer
                .dither(image, quantize)
                .convert_with(RGB::from_chroma_corrected_black_and_white);

            // Convert image back to normal...
            let (width, height) = image.size();
            let image = image::RgbImage::from_raw(width, height, image.raw_buf())
                .context("Could not convert back to a regular image")?;

            image.save("test.png")?;
        }
        Ok(())
    }
}

fn log_result(res: Result<()>) {
    if let Err(e) = res {
        println!("Error: {}", e);
    }
}

fn main() -> Result<()> {
    // Log in to Discord using a bot token from the environment
    let discord = Discord::from_bot_token(&env::var("DISCORD_TOKEN").context("Expected token")?)
        .context("login failed")?;

    // Establish and use a websocket connection
    let (mut connection, _) = discord.connect().context("connect failed")?;

    let mut handler = Handler::new()?;

    println!("Ready.");
    loop {
        match connection.recv_event() {
            Ok(Event::MessageCreate(message)) => {
                log_result(handler.handle(message));

                /*
                if message.content == "!test" {
                    let _ = discord.send_message(
                        message.channel_id,
                        "This is a reply to the test.",
                        "",
                        false,
                    );
                } else if message.content == "!quit" {
                    println!("Quitting.");
                    break;
                }
                */
            }
            Ok(_) => {}
            Err(discord::Error::Closed(code, body)) => {
                println!("Gateway closed on us with code {:?}: {}", code, body);
                break;
            }
            Err(err) => println!("Receive error: {:?}", err),
        }
    }
    Ok(())
}
