use anyhow::{Context, Result};
use discord::model::Event;
use discord::{model::Message, Discord};
use dither::prelude::*;
use escposify::{img::Image as EscImage, printer::Printer};
use hyper::net::HttpsConnector;
use hyper::Client;
use hyper_native_tls::NativeTlsClient;
use image::GenericImageView;
use pos58_usb::POS58USB;
use std::env;
use std::io::Read;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};

struct Handler {
    client: Client,
    ditherer: Ditherer<'static>,
    printer: Sender<image::RgbImage>,
}

fn printer_thread(receiver: Receiver<image::RgbImage>) -> Result<()> {
    let mut usb_context = libusb::Context::new().expect("Failed to create LibUSB context.");
    let mut device = POS58USB::new(&mut usb_context, std::time::Duration::from_secs(90))
        .expect("Failed to connect to printer");
    let mut printer = Printer::new(&mut device, None, None);
    while let Ok(image) = receiver.recv() {
        let image = EscImage::from(image::DynamicImage::ImageRgb8(image));
        printer
            .chain_align("ct")?
            .chain_bit_image(&image, None)?
            .flush()?;
    }
    eprintln!("Note: printer thread stopped.");
    Ok(())
}

impl Handler {
    pub fn new() -> Result<Self> {
        let ssl = NativeTlsClient::new()?;
        let connector = HttpsConnector::new(ssl);
        let client = hyper::Client::with_connector(connector);
        let ditherer = Ditherer::from_str("floyd")?;
        let (printer, receiver) = mpsc::channel();
        std::thread::spawn(move || log_result(printer_thread(receiver)));
        Ok(Self {
            client,
            ditherer,
            printer,
        })
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

            // Send image to the printer thread
            // NOTE: This is a fatal error; the expect is intentional!
            self.printer.send(image).expect("Printer thread died"); 
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
