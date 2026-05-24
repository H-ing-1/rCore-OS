use rcore_os_virtio_snd::{
    PcmFormat, StreamDirection, VirtioSound, VirtioSndConfig,
};

fn main() {
    let config = VirtioSndConfig {
        jacks: 1,
        streams: 2,
        chmaps: 1,
    };

    let mut driver = VirtioSound::new(config, 0b111);
    driver
        .register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44_100)
        .expect("register stream");
    driver.setup_and_start(0).expect("start stream");

    let pcm = [0u8; 256];
    let written = driver.write_audio_frames(0, &pcm).expect("write pcm");
    println!("queued {} bytes", written);

    driver.stop_and_release(0).expect("stop stream");
}
