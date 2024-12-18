use byteorder::{ByteOrder, LittleEndian};
use coreaudio::audio_unit::macos_helpers::{
    audio_unit_from_device_id, get_device_id_from_name, get_device_name,
};
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, Element, Scope};
use coreaudio::sys::{
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMaster,
    kAudioObjectPropertyScopeGlobal, AudioDeviceID, AudioObjectGetPropertyData,
    AudioObjectPropertyAddress,
};
use coreaudio_sys::{
    kAudioDevicePropertyDeviceUID, kAudioHardwareNoError, kAudioHardwarePropertyTranslateUIDToBox,
    kAudioObjectPropertyIdentify, kAudioObjectPropertyName, kCFAllocatorDefault,
    kCFStringEncodingUTF8, AudioObjectPropertySelector, AudioObjectSetPropertyData, CFRelease,
    CFStringCreateWithCString, CFStringRef, CFTypeRef,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use log::info;
use std::ffi::{CStr, CString};
use std::mem;
use std::os::raw::{c_char, c_void};
use std::path::Path;
use std::process::Stdio;
use std::ptr::null;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use tauri::async_runtime::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::recorder::RecordingOptions;
use crate::utils::ffmpeg_path_as_str;

unsafe impl Send for MediaRecorder {}
unsafe impl Sync for MediaRecorder {}

pub struct MediaRecorder {
    pub options: Option<RecordingOptions>,
    ffmpeg_audio_input_process: Option<tokio::process::Child>,
    ffmpeg_audio_input_stdin: Option<Arc<Mutex<Option<tokio::process::ChildStdin>>>>,
    ffmpeg_audio_output_process: Option<tokio::process::Child>,
    ffmpeg_audio_output_stdin: Option<Arc<Mutex<Option<tokio::process::ChildStdin>>>>,
    device_name: Option<String>,
    input_stream: Option<cpal::Stream>,
    output_audio_unit: Option<AudioUnit>,
    audio_input_channel_sender: Option<mpsc::Sender<Vec<u8>>>,
    audio_input_channel_receiver: Option<mpsc::Receiver<Vec<u8>>>,
    audio_output_channel_sender: Option<mpsc::Sender<Vec<u8>>>,
    audio_output_channel_receiver: Option<mpsc::Receiver<Vec<u8>>>,
    should_stop: Arc<AtomicBool>,
    start_time: Option<Instant>,
    audio_file_path: Option<String>,
}

pub enum DeviceType {
    AudioInput,
    AudioOutput,
}

impl MediaRecorder {
    pub fn new() -> Self {
        MediaRecorder {
            options: None,
            ffmpeg_audio_input_process: None,
            ffmpeg_audio_input_stdin: None,
            ffmpeg_audio_output_process: None,
            ffmpeg_audio_output_stdin: None,
            device_name: None,
            input_stream: None,
            output_audio_unit: None,
            audio_input_channel_sender: None,
            audio_input_channel_receiver: None,
            audio_output_channel_sender: None,
            audio_output_channel_receiver: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            start_time: None,
            audio_file_path: None,
        }
    }

    pub async fn start_media_recording(
        &mut self,
        options: RecordingOptions,
        audio_input_chunks_dir: &Path,
        audio_output_chunks_dir: &Path,
        input_device_id: Option<AudioDeviceID>,
        output_device_id: Option<AudioDeviceID>,
    ) -> Result<(), String> {
        self.options = Some(options.clone());

        let (audio_input_tx, audio_input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2048);
        let (audio_output_tx, audio_output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2048);

        let audio_input_start_time = Arc::new(Mutex::new(None));
        let audio_output_start_time = Arc::new(Mutex::new(None));

        self.audio_input_channel_sender = Some(audio_input_tx);
        self.audio_input_channel_receiver = Some(audio_input_rx);

        self.audio_output_channel_sender = Some(audio_output_tx);
        self.audio_output_channel_receiver = Some(audio_output_rx);

        self.ffmpeg_audio_input_stdin = Some(Arc::new(Mutex::new(None)));
        self.ffmpeg_audio_output_stdin = Some(Arc::new(Mutex::new(None)));

        let audio_input_channel_sender = self.audio_input_channel_sender.clone();
        let audio_output_channel_sender = self.audio_output_channel_sender.clone();

        let audio_input_channel_receiver =
            Arc::new(Mutex::new(self.audio_input_channel_receiver.take()));

        let audio_output_channel_receiver =
            Arc::new(Mutex::new(self.audio_output_channel_receiver.take()));
        let input_device_name =
            get_device_name(input_device_id.unwrap()).expect("failed to get input device name");
        let input_device = get_device(Some(&input_device_name), DeviceType::AudioInput);
        let output_device_name =
            get_device_name(output_device_id.unwrap()).expect("failed to get output device name");
        let output_device = get_device(Some(&output_device_name), DeviceType::AudioInput);

        let input_config: cpal::SupportedStreamConfig = input_device
            .supported_input_configs()
            .expect("Failed to get supported input configs")
            .find(|c| {
                c.sample_format() == SampleFormat::F32
                    || c.sample_format() == SampleFormat::I16
                    || c.sample_format() == SampleFormat::I8
                    || c.sample_format() == SampleFormat::I32
            })
            .unwrap_or_else(|| {
                input_device
                    .supported_input_configs()
                    .expect("Failed to get supported input configs")
                    .next()
                    .expect("No supported input config")
            })
            .with_max_sample_rate();

        info!("Building input stream...");
        info!("input_device {}", input_device.name().unwrap());
        let stream_result: Result<cpal::Stream, cpal::BuildStreamError> = build_audio_stream(
            &input_config,
            &input_device,
            audio_input_start_time,
            audio_input_channel_sender,
        );

        let stream = stream_result.map_err(|_| "Failed to build input stream")?;
        self.input_stream = Some(stream);
        self.trigger_play_input()?;

        info!("Building output stream..");
        info!(
            "output_device id: {} name: {}",
            output_device_id.unwrap(),
            output_device_name
        );
        let result = build_coreaudio_audio_stream(
            output_device_id.unwrap(),
            44100.0,
            2,
            audio_output_start_time,
            audio_output_channel_sender,
        );

        let output_audio_unit =
            result.map_err(|err| format!("Failed to build input stream: {}", err))?;
        self.output_audio_unit = Some(output_audio_unit);
        self.trigger_play_output()?;

        info!("Starting audio recording and processing...");

        let audio_input_file_path = audio_input_chunks_dir.to_str().unwrap();
        let audio_output_file_path = audio_output_chunks_dir.to_str().unwrap();

        let input_process = self
            .create_and_start_recording_process(
                &input_device,
                input_config.sample_rate().0,
                input_config.channels(),
                match input_config.sample_format() {
                    SampleFormat::I8 => "s8",
                    SampleFormat::I16 => "s16le",
                    SampleFormat::I32 => "s32le",
                    SampleFormat::F32 => "f32le",
                    _ => panic!("Unsupported sample format."),
                },
                self.ffmpeg_audio_input_stdin.clone(),
                &audio_input_file_path,
                Some(&input_device_name),
                audio_input_channel_receiver,
            )
            .await?;

        info!("created input recording process!");

        self.ffmpeg_audio_input_process = input_process;
        let output_process = self
            .create_and_start_recording_process(
                &output_device,
                44100.0 as u32,
                1,
                "f32le",
                self.ffmpeg_audio_output_stdin.clone(),
                &audio_output_file_path,
                Some(&output_device_name),
                audio_output_channel_receiver,
            )
            .await?;

        self.ffmpeg_audio_output_process = output_process;

        info!("created output recording process!");

        info!("End of the start_audio_recording function");

        Ok(())
    }

    async fn create_and_start_recording_process(
        &mut self,
        device: &Device,
        sample_rate: u32,
        channels: u16,
        sample_format: &str,
        ffmpeg_audio_stdin: Option<Arc<Mutex<Option<tokio::process::ChildStdin>>>>,
        audio_file_path: &str,
        custom_device: Option<&str>,
        audio_channel_receiver: Arc<Mutex<Option<mpsc::Receiver<Vec<u8>>>>>,
    ) -> Result<Option<Child>, String> {
        // let sample_rate: u32 = device_config.sample_rate().0;
        // let channels: u16 = device_config.channels();
        // let sample_format: &str = match device_config.sample_format() {
        //     SampleFormat::I8 => "s8",
        //     SampleFormat::I16 => "s16le",
        //     SampleFormat::I32 => "s32le",
        //     SampleFormat::F32 => "f32le",
        //     _ => panic!("Unsupported sample format."),
        // };

        info!("Sample rate: {}", sample_rate);
        info!("Channels: {}", channels);
        info!("Sample format: {}", sample_format);

        let ffmpeg_binary_path_str = ffmpeg_path_as_str().unwrap().to_owned();

        info!("FFmpeg binary path: {}", ffmpeg_binary_path_str);

        let audio_file_path_owned = audio_file_path.to_owned();

        let ffmpeg_audio_stdin = ffmpeg_audio_stdin.clone();

        let audio_output_chunk_pattern = format!("{}/audio_recording_%03d.wav", audio_file_path);
        let audio_segment_list_filename = format!("{}/segment_list.txt", audio_file_path);

        let mut audio_filters = Vec::new();

        if channels > 2 {
            audio_filters.push("pan=stereo|FL=FL+0.5*FC|FR=FR+0.5*FC");
        }

        audio_filters.push("loudnorm");

        let ffmpeg_audio_command: Vec<String> = vec![
            "-f",
            sample_format,
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-thread_queue_size",
            "4096",
            "-i",
            "pipe:0",
            "-af",
            "aresample=async=1:min_hard_comp=0.100000:first_pts=0:osr=16000",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            "-async",
            "1",
            "-f",
            "segment",
            "-segment_time",
            "3",
            "-segment_time_delta",
            "0.01",
            "-segment_list",
            &audio_segment_list_filename,
            "-reset_timestamps",
            "1",
            &audio_output_chunk_pattern,
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect();

        info!("FFmpeg audio command: {:?}", ffmpeg_audio_command.join(" "));

        info!("Starting FFmpeg audio process...");

        let mut audio_stdin: Option<ChildStdin> = None;
        let mut audio_child: Option<Child> = None;

        if custom_device != Some("None") {
            let (child, stdin) = self
                .start_audio_ffmpeg_processes(&ffmpeg_binary_path_str, &ffmpeg_audio_command)
                .await
                .map_err(|e| e.to_string())?;
            audio_child = Some(child);
            audio_stdin = Some(stdin);
            info!("Audio input process started");
        }

        if let Some(ffmpeg_audio_input_stdin) = &ffmpeg_audio_stdin {
            let mut audio_input_stdin_lock = ffmpeg_audio_input_stdin.lock().await;
            *audio_input_stdin_lock = audio_stdin;
            drop(audio_input_stdin_lock);
            info!("Audio input stdin set");
        }

        if custom_device != Some("None") {
            info!("Starting audio channel receivers...");
            tokio::spawn(async move {
                while let Some(bytes) = &audio_channel_receiver
                    .lock()
                    .await
                    .as_mut()
                    .unwrap()
                    .recv()
                    .await
                {
                    if let Some(audio_input_stdin_arc) = &ffmpeg_audio_stdin {
                        let mut audio_stdin_guard = audio_input_stdin_arc.lock().await;
                        if let Some(ref mut stdin) = *audio_stdin_guard {
                            stdin
                                .write_all(&bytes)
                                .await
                                .expect("Failed to write audio data to FFmpeg stdin");
                        }
                        drop(audio_stdin_guard);
                    }
                }
            });
        }

        self.start_time = Some(Instant::now());
        self.audio_file_path = Some(audio_file_path_owned);
        self.device_name = Some(device.name().expect("Failed to get device name"));
        Ok(audio_child)
    }

    pub fn trigger_play_input(&mut self) -> Result<(), &'static str> {
        if let Some(ref mut stream) = self.input_stream {
            stream.play().map_err(|_| "Failed to play stream")?;
            info!("Audio recording playing.");
        } else {
            return Err("Starting the recording did not work");
        }

        Ok(())
    }

    pub fn trigger_play_output(&mut self) -> Result<(), &'static str> {
        if let Some(ref mut output_audio_unit) = self.output_audio_unit {
            output_audio_unit
                .start()
                .map_err(|_| "Failed to play stream")?;
            info!("Audio recording playing.");
        } else {
            return Err("Starting the recording did not work");
        }

        Ok(())
    }

    pub async fn stop_media_recording(&mut self) -> Result<(), String> {
        if let Some(start_time) = self.start_time {
            let segment_duration = Duration::from_secs(3);
            let recording_duration = start_time.elapsed();
            let expected_segments = recording_duration.as_secs() / segment_duration.as_secs();
            let audio_file_path = self
                .audio_file_path
                .as_ref()
                .ok_or("Audio file path not set")?;
            let audio_segment_list_filename = format!("{}/segment_list.txt", audio_file_path);

            loop {
                let audio_segments =
                    std::fs::read_to_string(&audio_segment_list_filename).unwrap_or_default();

                let audio_segment_count = audio_segments.lines().count();

                if audio_segment_count >= expected_segments as usize {
                    info!("All segments generated");
                    break;
                }

                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }

        if let Some(ref ffmpeg_audio_stdin) = self.ffmpeg_audio_input_stdin {
            let mut audio_stdin_guard = ffmpeg_audio_stdin.lock().await;
            if let Some(mut audio_stdin) = audio_stdin_guard.take() {
                if let Err(e) = audio_stdin.write_all(b"q\n").await {
                    info!("Failed to send 'q' to audio FFmpeg process: {}", e);
                }
                let _ = audio_stdin.shutdown().await.map_err(|e| e.to_string());
            }
        }

        if let Some(ref ffmpeg_audio_stdin) = self.ffmpeg_audio_output_stdin {
            let mut audio_stdin_guard = ffmpeg_audio_stdin.lock().await;
            if let Some(mut audio_stdin) = audio_stdin_guard.take() {
                if let Err(e) = audio_stdin.write_all(b"q\n").await {
                    info!("Failed to send 'q' to audio FFmpeg process: {}", e);
                }
                let _ = audio_stdin.shutdown().await.map_err(|e| e.to_string());
            }
        }

        self.should_stop.store(true, Ordering::SeqCst);

        if let Some(sender) = self.audio_input_channel_sender.take() {
            drop(sender);
        }
        if let Some(sender) = self.audio_output_channel_sender.take() {
            drop(sender);
        }

        if let Some(ref mut stream) = self.input_stream {
            stream.pause().map_err(|_| "Failed to pause stream")?;
            info!("Audio recording paused.");
        } else {
            return Err("Original recording was not started".to_string());
        }

        if let Some(ref mut output_audio_unit) = self.output_audio_unit {
            output_audio_unit
                .stop()
                .map_err(|_| "Failed to pause stream")?;
            info!("Audio recording paused.");
        } else {
            return Err("Original recording was not started".to_string());
        }

        if let Some(process) = &mut self.ffmpeg_audio_input_process {
            let _ = process.kill().await.map_err(|e| e.to_string());
        }

        info!("Audio recording stopped.");
        Ok(())
    }

    async fn start_audio_ffmpeg_processes(
        &self,
        ffmpeg_binary_path: &str,
        audio_ffmpeg_command: &[String],
    ) -> Result<(Child, ChildStdin), std::io::Error> {
        info!("start_audio_ffmpeg_processes");
        let mut audio_process = start_recording_process(ffmpeg_binary_path, audio_ffmpeg_command)
            .await
            .map_err(|e| {
                info!("Failed to start audio recording process: {}", e);
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
            })?;

        let audio_stdin = audio_process.stdin.take().ok_or_else(|| {
            info!("Failed to take audio stdin");
            std::io::Error::new(std::io::ErrorKind::Other, "Failed to take audio stdin")
        })?;

        Ok((audio_process, audio_stdin))
    }

    fn set_object_name(
        box_device_id: AudioDeviceID,
        action: &str,
        new_name: &str,
    ) -> Result<(), std::io::Error> {
        let command = format!("{}{}", action, new_name);
        let set_name_address = AudioObjectPropertyAddress {
            mSelector: kAudioObjectPropertyName,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyIdentify,
        };
        unsafe {
            let c_string_new_name = CString::new(command).unwrap();
            let cf_new_name = CFStringCreateWithCString(
                kCFAllocatorDefault,
                c_string_new_name.as_ptr(),
                kCFStringEncodingUTF8,
            );
            let status = AudioObjectSetPropertyData(
                box_device_id,
                &set_name_address,
                0,
                null(),
                std::mem::size_of::<CFStringRef>() as u32,
                &cf_new_name as *const _ as *const c_void,
            );

            if coreaudio::Error::from_os_status(status).is_err() {
                info!(
                    "Error setting object name to device ID: {}",
                    coreaudio::Error::from_os_status(status).unwrap_err()
                );
            } else {
                info!(
                    "Successfully set object name to device ID: {}",
                    box_device_id
                );
            }
            CFRelease(cf_new_name as CFTypeRef);
        }
        Ok(())
    }
}

pub fn audio_device_id_for_uid(
    device_uid: &str,
    selector: AudioObjectPropertySelector,
) -> AudioDeviceID {
    let mut device_id: coreaudio_sys::AudioDeviceID = coreaudio_sys::kAudioObjectUnknown;
    let mut size = std::mem::size_of::<coreaudio_sys::AudioDeviceID>() as u32;
    let property_address = coreaudio_sys::AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: coreaudio_sys::kAudioObjectPropertyScopeGlobal,
        mElement: coreaudio_sys::kAudioObjectPropertyElementMaster,
    };
    info!("Property address: {:?}", property_address);
    let uid = CString::new(device_uid).unwrap();
    let cf_uid = unsafe {
        CFStringCreateWithCString(kCFAllocatorDefault, uid.as_ptr(), kCFStringEncodingUTF8)
    };
    info!("UID: {:?}", uid);
    info!("CFString UID: {:?}", cf_uid);
    unsafe {
        let status = coreaudio_sys::AudioObjectGetPropertyData(
            coreaudio_sys::kAudioObjectSystemObject,
            &property_address,
            std::mem::size_of::<CFStringRef>() as u32,
            &cf_uid as *const _ as *const c_void,
            &mut size,
            &mut device_id as *mut _ as *mut c_void,
        );
        if coreaudio::Error::from_os_status(status).is_err() {
            info!(
                "Error translating UID to device ID: {}",
                coreaudio::Error::from_os_status(status).unwrap_err()
            );
        } else {
            info!("Successfully translated UID to device ID: {}", device_id);
        }
        CFRelease(cf_uid as CFTypeRef);
    }
    device_id
}

fn audio_device_id_for_box_id(uid: &str) -> AudioDeviceID {
    audio_device_id_for_uid(&uid, kAudioHardwarePropertyTranslateUIDToBox)
}

fn audio_device_id_for_device_uid(uid: &str) -> AudioDeviceID {
    audio_device_id_for_uid(&uid, kAudioHardwarePropertyTranslateUIDToDevice)
}

fn audio_device_uid_for_device_id(device_id: AudioDeviceID) -> Result<String, coreaudio::Error> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    macro_rules! try_status_or_return {
        ($status:expr) => {
            if $status != kAudioHardwareNoError as i32 {
                return Err(coreaudio::Error::Unknown($status));
            }
        };
    }

    let device_name: core_foundation_sys::string::CFStringRef = null();
    let data_size = mem::size_of::<core_foundation_sys::string::CFStringRef>();
    let c_str = unsafe {
        let status = AudioObjectGetPropertyData(
            device_id,
            &property_address as *const _,
            0,
            null(),
            &data_size as *const _ as *mut _,
            &device_name as *const _ as *mut _,
        );
        try_status_or_return!(status);

        let c_string: *const c_char =
            core_foundation_sys::string::CFStringGetCStringPtr(device_name, kCFStringEncodingUTF8);
        if c_string.is_null() {
            let status = AudioObjectGetPropertyData(
                device_id,
                &property_address as *const _,
                0,
                null(),
                &data_size as *const _ as *mut _,
                &device_name as *const _ as *mut _,
            );
            try_status_or_return!(status);
            let mut buf: [i8; 255] = [0; 255];
            let result = core_foundation_sys::string::CFStringGetCString(
                device_name,
                buf.as_mut_ptr(),
                buf.len() as _,
                kCFStringEncodingUTF8,
            );
            if result == 0 {
                return Err(coreaudio::Error::Unknown(result as i32));
            }
            let name: &CStr = CStr::from_ptr(buf.as_ptr());
            return Ok(name.to_str().unwrap().to_owned());
        }
        CStr::from_ptr(c_string as *mut _)
    };
    Ok(c_str.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn set_target_output_device(device: String) -> Result<(), String> {
    let proxy_audio_box = audio_device_id_for_box_id("ProxyAudioBox_UID");
    let device_id = get_device_id_from_name(&device, false).expect("failed to get device id");
    let device_uid = audio_device_uid_for_device_id(device_id).expect("failed to get device uid");
    MediaRecorder::set_object_name(proxy_audio_box, "outputDevice=", &device_uid)
        .map_err(|err| err.to_string())
}

use tokio::io::{AsyncBufReadExt, BufReader};

fn get_device(custom_device: Option<&str>, device_type: DeviceType) -> Device {
    info!("Custom device: {:?}", custom_device);

    let host = cpal::default_host();
    let all_devices = host.devices().expect("Failed to get devices");
    let mut devices = all_devices.filter_map(|device| match device_type {
        DeviceType::AudioInput => {
            let supported_input_configs = device.supported_input_configs();
            if supported_input_configs.is_ok() && supported_input_configs.unwrap().count() > 0 {
                Some(device)
            } else {
                None
            }
        }
        DeviceType::AudioOutput => {
            let supported_output_configs = device.supported_output_configs();
            if supported_output_configs.is_ok() && supported_output_configs.unwrap().count() > 0 {
                Some(device)
            } else {
                None
            }
        }
    });

    let device = if let Some(custom_device_name) = custom_device {
        devices
            .find(|d| {
                d.name()
                    .map(|name| name == custom_device_name)
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| {
                host.default_input_device()
                    .expect("No default input device available")
            })
    } else {
        host.default_input_device()
            .expect("No default input device available")
    };

    info!(
        "Using audio input device: {}",
        device.name().expect("Failed to get device name")
    );
    device
}

fn build_audio_stream(
    stream_config: &cpal::SupportedStreamConfig,
    device: &Device,
    audio_start_time: Arc<Mutex<Option<Instant>>>,
    audio_channel_sender: Option<mpsc::Sender<Vec<u8>>>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let err_fn = move |err| {
        info!("an error occurred on stream: {}", err);
    };

    let stream_result: Result<cpal::Stream, cpal::BuildStreamError> =
        match stream_config.sample_format() {
            SampleFormat::I8 => device.build_input_stream(
                &stream_config.config(),
                {
                    let audio_start_time = Arc::clone(&audio_start_time);
                    move |data: &[i8], _: &_| {
                        let mut first_frame_time_guard = audio_start_time.try_lock();

                        let bytes = data.iter().map(|&sample| sample as u8).collect::<Vec<u8>>();
                        if let Some(sender) = &audio_channel_sender {
                            if sender.try_send(bytes).is_err() {
                                info!("Channel send error. Dropping data.");
                            }
                        }

                        if let Ok(ref mut start_time_option) = first_frame_time_guard {
                            if start_time_option.is_none() {
                                **start_time_option = Some(Instant::now());

                                info!("Audio start time captured");
                            }
                        }
                    }
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &stream_config.config(),
                {
                    let audio_start_time = Arc::clone(&audio_start_time);
                    move |data: &[i16], _: &_| {
                        let mut first_frame_time_guard = audio_start_time.try_lock();

                        let mut bytes = vec![0; data.len() * 2];
                        LittleEndian::write_i16_into(data, &mut bytes);
                        if let Some(sender) = &audio_channel_sender {
                            if sender.try_send(bytes).is_err() {
                                info!("Channel send error. Dropping data.");
                            }
                        }

                        if let Ok(ref mut start_time_option) = first_frame_time_guard {
                            if start_time_option.is_none() {
                                **start_time_option = Some(Instant::now());

                                info!("Audio start time captured");
                            }
                        }
                    }
                },
                err_fn,
                None,
            ),
            SampleFormat::I32 => device.build_input_stream(
                &stream_config.config(),
                {
                    let audio_start_time = Arc::clone(&audio_start_time);
                    move |data: &[i32], _: &_| {
                        let mut first_frame_time_guard = audio_start_time.try_lock();

                        let mut bytes = vec![0; data.len() * 2];
                        LittleEndian::write_i32_into(data, &mut bytes);
                        if let Some(sender) = &audio_channel_sender {
                            if sender.try_send(bytes).is_err() {
                                info!("Channel send error. Dropping data.");
                            }
                        }

                        if let Ok(ref mut start_time_option) = first_frame_time_guard {
                            if start_time_option.is_none() {
                                **start_time_option = Some(Instant::now());

                                info!("Audio start time captured");
                            }
                        }
                    }
                },
                err_fn,
                None,
            ),
            SampleFormat::F32 => device.build_input_stream(
                &stream_config.config(),
                {
                    let audio_start_time = Arc::clone(&audio_start_time);
                    move |data: &[f32], _: &_| {
                        let mut first_frame_time_guard = audio_start_time.try_lock();

                        let mut bytes = vec![0; data.len() * 4];
                        LittleEndian::write_f32_into(data, &mut bytes);
                        if let Some(sender) = &audio_channel_sender {
                            if sender.try_send(bytes).is_err() {
                                info!("Channel send error. Dropping data.");
                            }
                        }

                        if let Ok(ref mut start_time_option) = first_frame_time_guard {
                            if start_time_option.is_none() {
                                **start_time_option = Some(Instant::now());

                                info!("Audio start time captured");
                            }
                        }
                    }
                },
                err_fn,
                None,
            ),
            _sample_format => Err(cpal::BuildStreamError::DeviceNotAvailable),
        };
    stream_result
}

type S = f32;
const SAMPLE_FORMAT: coreaudio::audio_unit::SampleFormat = coreaudio::audio_unit::SampleFormat::F32;
fn build_coreaudio_audio_stream(
    device_id: AudioDeviceID,
    sample_rate: f64,
    _channels: u32,
    audio_start_time: Arc<Mutex<Option<Instant>>>,
    audio_channel_sender: Option<mpsc::Sender<Vec<u8>>>,
) -> Result<AudioUnit, coreaudio::Error> {
    info!("Input device: {}", get_device_name(device_id).unwrap());
    let format_flag = match SAMPLE_FORMAT {
        coreaudio::audio_unit::SampleFormat::F32 => {
            coreaudio::audio_unit::audio_format::LinearPcmFlags::IS_FLOAT
                | coreaudio::audio_unit::audio_format::LinearPcmFlags::IS_PACKED
        }
        coreaudio::audio_unit::SampleFormat::I32
        | coreaudio::audio_unit::SampleFormat::I16
        | coreaudio::audio_unit::SampleFormat::I8 => {
            coreaudio::audio_unit::audio_format::LinearPcmFlags::IS_SIGNED_INTEGER
                | coreaudio::audio_unit::audio_format::LinearPcmFlags::IS_PACKED
        }
        _ => {
            unimplemented!("Please use one of the packed formats");
        }
    };

    let in_stream_format = coreaudio::audio_unit::StreamFormat {
        sample_rate: sample_rate,
        sample_format: SAMPLE_FORMAT,
        flags: format_flag,
        channels: 1,
    };

    let mut input_audio_unit = audio_unit_from_device_id(device_id, true)?;
    let id = coreaudio::sys::kAudioUnitProperty_StreamFormat;
    let asbd = in_stream_format.to_asbd();
    input_audio_unit.set_property(id, Scope::Output, Element::Input, Some(&asbd))?;

    type Args = render_callback::Args<data::Interleaved<f32>>;
    // Define input callback
    let callback = move |args: render_callback::Args<data::Interleaved<f32>>| {
        let Args { data, .. } = args;
        let audio_start_time = Arc::clone(&audio_start_time);

        let mut first_frame_time_guard = audio_start_time.try_lock();

        if let Some(sender) = &audio_channel_sender {
            let mut bytes = vec![0; data.buffer.len() * 4];
            LittleEndian::write_f32_into(data.buffer, &mut bytes);
            if sender.try_send(bytes).is_err() {
                info!("Channel send error. Dropping data.");
            }
        }

        if let Ok(ref mut start_time_option) = first_frame_time_guard {
            if start_time_option.is_none() {
                **start_time_option = Some(Instant::now());

                info!("Audio start time captured");
            }
        }

        Ok(())
    };

    input_audio_unit.set_input_callback(callback)?;

    Ok(input_audio_unit)
}

async fn start_recording_process(
    ffmpeg_binary_path_str: &str,
    args: &[String],
) -> Result<tokio::process::Child, std::io::Error> {
    info!("start_recording_process");
    let mut process = Command::new(ffmpeg_binary_path_str)
        .args(args)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(process_stderr) = process.stderr.take() {
        tokio::spawn(async move {
            let mut process_reader = BufReader::new(process_stderr).lines();
            while let Ok(Some(line)) = process_reader.next_line().await {
                info!("FFmpeg process STDERR: {}", line);
            }
        });
    }

    Ok(process)
}
