use discord::model::Discord;

pub struct CameraHandler<'a> {
    device: Device,
    stream: Stream<'a>,
}

impl CameraHandler {
    pub fn new(device: &mut Device) -> Result<Self> {

        // Let's say we want to explicitly request another format
        let mut fmt = device.format().context("Failed to read format")?;
        fmt.width = 1280;
        fmt.height = 720;
        fmt.fourcc = FourCC::new(b"MJPG");
        device.set_format(&fmt).context("Failed to write format")?;

        let mut stream = Stream::with_buffers(&mut device, Type::VideoCapture, 4)
            .context("Failed to create buffer stream")?;

        Ok(Self {
            device,
            stream,
        })
    }

    pub fn handle(&mut self, &mut Discord) -> Result<()> {
        let (buf, meta) = stream.next().context("Camera stream closed")?;
        Ok(())
    }
}


fn main() {

    // The actual format chosen by the device driver may differ from what we
    // requested! Print it out to get an idea of what is actually used now.
    println!("Format in use:\n{}", fmt);

    // Now we'd like to capture some frames!
    // First, we need to create a stream to read buffers from. We choose a
    // mapped buffer stream, which uses mmap to directly access the device
    // frame buffer. No buffers are copied nor allocated, so this is actually
    // a zero-copy operation.

    // To achieve the best possible performance, you may want to use a
    // UserBufferStream instance, but this is not supported on all devices,
    // so we stick to the mapped case for this example.
    // Please refer to the rustdoc docs for a more detailed explanation about
    // buffer transfers.

    // Create the stream, which will internally 'allocate' (as in map) the
    // number of requested buffers for us.

    // At this point, the stream is ready and all buffers are setup.
    // We can now read frames (represented as buffers) by iterating through
    // the stream. Once an error condition occurs, the iterator will return
    // None.
    loop {
        println!(
            "Buffer size: {}, seq: {}, timestamp: {}",
            buf.len(),
            meta.sequence,
            meta.timestamp
        );

        // To process the captured data, you can pass it somewhere else.
        // If you want to modify the data or extend its lifetime, you have to
        // copy it. This is a best-effort tradeoff solution that allows for
        // zero-copy readers while enforcing a full clone of the data for
        // writers.
    }
}
