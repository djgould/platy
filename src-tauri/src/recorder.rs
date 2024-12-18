use coreaudio_sys::AudioObjectID;
use log::info;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tauri::async_runtime::Mutex;
use tauri::State;
use tokio::process::Command;

// Removed unused imports
// use mac_notification_sys::{get_bundle_identifier_or_default, send_notification, set_application};
// use crate::commands::conversation;
// use crate::summarize::{generate_action_items, generate_title, summarize};
use crate::media::MediaRecorder;
use crate::summarize::summarize_and_write;
use crate::transcribe::{load_transcription, transcribe_wav_file_and_write};
use crate::utils::ffmpeg_path_as_str;
use crate::DeviceState;

pub struct RecordingState {
    pub media_process: Option<MediaRecorder>,
    pub recording_options: Option<RecordingOptions>,
    pub shutdown_flag: Arc<AtomicBool>,
    pub audio_uploading_finished: Arc<AtomicBool>,
    pub data_dir: Option<PathBuf>,
    pub conversation_id: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordingOptions {
    pub user_id: String,
    pub audio_input_name: String,
    pub audio_output_name: String,
}

pub async fn _start_recording(
    state: State<'_, Arc<tauri::async_runtime::Mutex<RecordingState>>>,
    device_state: State<'_, Arc<tauri::async_runtime::Mutex<DeviceState>>>,
    options: RecordingOptions,
    conversation_id: u32,
) -> Result<(), String> {
    let mut state_guard = state.lock().await;
    let device_state_guard = device_state.lock().await;
    // send_notification("Platy", None, "Starting recording", None).unwrap();

    let shutdown_flag = Arc::new(AtomicBool::new(false));

    let data_dir = state_guard
        .data_dir
        .as_ref()
        .ok_or("Data directory is not set in the recording state".to_string())?
        .clone();

    info!("data_dir: {:?}", data_dir);

    state_guard.conversation_id = Some(conversation_id);

    let output_dir = data_dir
        .join("chunks/audio")
        .join(conversation_id.to_string());
    let audio_input_chunks_dir = output_dir.join("input");
    let audio_output_chunks_dir = output_dir.join("output");

    clean_and_create_dir(&output_dir)?;
    clean_and_create_dir(&audio_input_chunks_dir)?;
    clean_and_create_dir(&audio_output_chunks_dir)?;

    let media_recording_preparation = prepare_media_recording(
        &options,
        &audio_input_chunks_dir,
        &audio_output_chunks_dir,
        device_state_guard.input_device_id,
        device_state_guard.aggregate_device_id,
    );
    let media_recording_result = media_recording_preparation
        .await
        .map_err(|e| e.to_string())?;

    state_guard.media_process = Some(media_recording_result);
    state_guard.recording_options = Some(options.clone());
    state_guard.shutdown_flag = shutdown_flag.clone();
    state_guard.audio_uploading_finished = Arc::new(AtomicBool::new(false));

    // let audio_upload = start_transcription_loop(
    //     audio_input_chunks_dir,
    //     audio_output_chunks_dir,
    //     shutdown_flag.clone(),
    //     state_guard.audio_uploading_finished.clone(),
    // );

    drop(state_guard);

    info!("Starting upload loops...");

    // match tokio::try_join!(audio_upload) {
    //     Ok(_) => {
    //         println!("Both upload loops completed successfully.");
    //     }
    //     Err(e) => {
    //         eprintln!("An error occurred: {}", e);
    //     }
    // }
    Ok(())
}

#[tauri::command]
pub async fn start_recording(
    state: State<'_, Arc<tauri::async_runtime::Mutex<RecordingState>>>,
    device_state: State<'_, Arc<tauri::async_runtime::Mutex<DeviceState>>>,
    options: RecordingOptions,
    conversation_id: u32,
) -> Result<(), String> {
    _start_recording(state, device_state, options, conversation_id).await
}
use tokio::io::AsyncBufReadExt;

async fn concat_segments(
    audio_chunks_dir: &PathBuf,
) -> Result<tokio::process::Child, std::io::Error> {
    let ffmpeg_binary_path_str = ffmpeg_path_as_str().unwrap().to_owned();

    let segment_list_path = audio_chunks_dir.join("segment_list.txt");

    // Read each line (segment file path) from the segment list file
    let segment_files: Vec<String> = match std::fs::read_to_string(&segment_list_path) {
        Ok(content) => Some(
            content
                .lines()
                .map(|s| s.trim().to_string())
                .collect::<Vec<String>>(),
        ),
        Err(e) => {
            info!("Failed to read segment list: {}", e);
            None
        }
    }
    .expect("Failed to read segment list. This should never happen. Please report this bug.");

    // Ensure there are segments to combine
    if segment_files.is_empty() {
        info!("No segments found to combine.");
    }

    let concat_file_path = audio_chunks_dir.join("concat.txt").clone();
    let combined_output_file_path = audio_chunks_dir.join("combined.wav");

    write_concat_file(&concat_file_path, &segment_files).expect("error writing concat file");

    let args = vec![
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        concat_file_path.to_str().unwrap(),
        "-c",
        "copy",
        combined_output_file_path.to_str().unwrap(),
    ];

    // Print the generated args for debugging
    info!("FFmpeg args: {:?}", args);

    let mut process = Command::new(ffmpeg_binary_path_str).args(args).spawn()?;

    if let Some(process_stderr) = process.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::BufReader;

            let mut process_reader = BufReader::new(process_stderr).lines();
            while let Ok(Some(line)) = process_reader.next_line().await {
                info!("FFmpeg process STDERR: {}", line);
            }
        });
    }

    process.wait().await?;
    Ok(process)
}

//ffmpeg -i stream1_combined.wav -i stream2_combined.wav -filter_complex "[0:a][1:a]amerge=inputs=2,pan=mono|c0=.5*c0+.5*c1[aout]" -map "[aout]" -c:a pcm_s16le output_mono.wav
async fn combine_segments(
    audio_chunks_dir: &PathBuf,
) -> Result<tokio::process::Child, std::io::Error> {
    let ffmpeg_binary_path_str = ffmpeg_path_as_str().unwrap().to_owned();

    let input_concat_file = audio_chunks_dir.join("input").join("combined.wav");
    let output_concat_file = audio_chunks_dir.join("output").join("combined.wav");
    let combined_output_file_path = audio_chunks_dir.join("combined.wav");

    let args = vec![
        "-i",
        input_concat_file.to_str().unwrap(),
        "-i",
        output_concat_file.to_str().unwrap(),
        "-filter_complex",
        "[0:a][1:a]amerge=inputs=2,pan=mono|c0=.5*c0+.5*c1[aout]",
        "-map",
        "[aout]",
        "-c:a",
        "pcm_s16le",
        combined_output_file_path.to_str().unwrap(),
    ];

    // Print the generated args for debugging
    info!("FFmpeg args: {:?}", args);

    let mut process = Command::new(ffmpeg_binary_path_str).args(args).spawn()?;

    if let Some(process_stderr) = process.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::BufReader;

            let mut process_reader = BufReader::new(process_stderr).lines();
            while let Ok(Some(line)) = process_reader.next_line().await {
                info!("FFmpeg process STDERR: {}", line);
            }
        });
    }

    process.wait().await?;
    Ok(process)
}

fn write_concat_file(concat_file_path: &PathBuf, segment_files: &Vec<String>) -> io::Result<()> {
    let mut output_file = File::create(concat_file_path)?;
    for segment_file in segment_files {
        output_file
            .write_all(format!("file '{}'\n", segment_file).as_bytes())
            .expect("error writing file");
    }
    Ok(())
}

pub async fn _stop_recording(
    handle: tauri::AppHandle,
    state: State<'_, Arc<Mutex<RecordingState>>>,
) -> Result<(), String> {
    let mut guard: tokio::sync::MutexGuard<RecordingState> = state.lock().await;

    info!("Stopping media recording...");

    guard.shutdown_flag.store(true, Ordering::SeqCst);

    if let Some(mut media_process) = guard.media_process.take() {
        info!("Stopping media recording...");
        media_process
            .stop_media_recording()
            .await
            .expect("Failed to stop media recording");
    }

    let conversation_id = guard
        .conversation_id
        .expect("can't stop recording without conversation id");

    // let is_local_mode = match dotenv_codegen::dotenv!("NEXT_PUBLIC_LOCAL_MODE") {
    //     "true" => true,
    //     _ => false,
    // };

    // if !is_local_mode {
    //     while !guard.audio_uploading_finished.load(Ordering::SeqCst) {
    //         println!("Waiting for uploads to finish...");
    //         tokio::time::sleep(Duration::from_millis(50)).await;
    //     }
    // }

    // while !guard.audio_uploading_finished.load(Ordering::SeqCst) {
    //     println!("Waiting for uploads to finish...");
    //     tokio::time::sleep(Duration::from_millis(50)).await;
    // }

    let data_dir = guard.data_dir.clone();
    let recording_dir = data_dir
        .expect("no data directory")
        .join("chunks/audio")
        .join(conversation_id.to_string());
    let input_dir = recording_dir.join("input");
    let output_dir = recording_dir.join("output");
    concat_segments(&input_dir)
        .await
        .map_err(|e| e.to_string())?;
    concat_segments(&output_dir)
        .await
        .map_err(|e| e.to_string())?;
    combine_segments(&recording_dir)
        .await
        .map_err(|e| e.to_string())?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    info!("combined segments..");

    let combined_audio_file = recording_dir.join("combined.wav");
    let transcription_output_file = recording_dir.join("transcription.json");
    let summary_output_file = recording_dir.join("summary.json");
    transcribe_wav_file_and_write(handle, &combined_audio_file, &transcription_output_file)
        .map_err(|e| e.to_string())?;
    let transcription = load_transcription(transcription_output_file)
        .await
        .expect("Failed to load transcription");
    summarize_and_write(
        transcription.full_text.join(" CHANGE_SPEAKER_TOKEN "),
        &summary_output_file,
    )
    .await
    .expect("Couldn't generate summary");

    // let action_items = generate_action_items(&summary);
    // let title = generate_title(&summary);
    info!("All recordings and uploads stopped.");

    Ok(())
}

#[tauri::command]
pub async fn stop_recording(
    handle: tauri::AppHandle,
    state: State<'_, Arc<Mutex<RecordingState>>>,
) -> Result<(), String> {
    _stop_recording(handle, state).await
}

#[tauri::command]
pub async fn delete_recording_data(
    state: State<'_, Arc<Mutex<RecordingState>>>,
    conversation_id: u64,
) -> Result<(), String> {
    let guard = state.lock().await;

    let data_dir = guard.data_dir.clone();

    let recording_dir = data_dir
        .expect("no data directory")
        .join("chunks/audio")
        .join(conversation_id.to_string());
    std::fs::remove_dir_all(&recording_dir).map_err(|e| e.to_string())?;
    Ok(())
}

fn clean_and_create_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        // Instead of just reading the directory, this will also handle subdirectories.
        std::fs::remove_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    if !dir.to_string_lossy().contains("screenshots") {
        let segment_list_path = dir.join("segment_list.txt");
        match File::open(&segment_list_path) {
            Ok(_) => Ok(()),
            Err(ref e) if e.kind() == ErrorKind::NotFound => {
                File::create(&segment_list_path).map_err(|e| e.to_string())?;
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    } else {
        Ok(())
    }
}

async fn prepare_media_recording(
    options: &RecordingOptions,
    audio_input_chunks_dir: &Path,
    audio_output_chunks_dir: &Path,
    audio_input_id: Option<AudioObjectID>,
    output_device_id: Option<AudioObjectID>,
) -> Result<MediaRecorder, String> {
    let mut media_recorder = MediaRecorder::new();
    media_recorder
        .start_media_recording(
            options.clone(),
            audio_input_chunks_dir,
            audio_output_chunks_dir,
            audio_input_id,
            output_device_id,
        )
        .await?;
    Ok(media_recorder)
}
