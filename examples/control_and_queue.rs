use rcore_os_virtio_snd::{
    PcmFormat, SndQueue, StreamDirection, VirtQueue, VirtioSound, VirtioSndConfig,
};

fn main() {
    let queue = VirtQueue::new(SndQueue::Control);
    println!("free descriptors: {}", queue.free_desc_count());

    let config = VirtioSndConfig {
        jacks: 0,
        streams: 1,
        chmaps: 1,
    };

    let mut driver = VirtioSound::new(config, 0b110);
    driver
        .register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 48_000)
        .expect("register stream");
    driver.setup_and_start(0).expect("setup stream");
    driver.set_volume(0, 128).expect("set volume");
    driver.toggle_mute(0).expect("toggle mute");

    let stream = driver.get_stream(0).expect("query stream");
    println!("muted={}, volume={}", stream.muted, stream.volume);
}
