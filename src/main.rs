use clap::Parser;
use serde::{Deserialize, Serialize};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json;
use std::fs;
use std::path::PathBuf;

const MAX_CHUNK_SIZE: usize = 4500; // A bit less than the 5000 byte API limit to be safe

/// A command-line tool to translate text files using the LibreTranslate API
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the input text file to translate
    #[arg(required = true)]
    input_file: PathBuf,

    /// Path to the output file (optional, prints to console if not provided)
    #[arg(short, long)]
    output_file: Option<PathBuf>,

    /// The LibreTranslate API endpoint URL
    #[arg(long, default_value = "https://translate.fedilab.app/translate")]
    api_url: String,

    /// Source language for translation (e.g., 'en')
    #[arg(short, long, default_value = "en")]
    source: String,

    /// Target language for translation (e.g., 'hu')
    #[arg(short, long, default_value = "hu")]
    target: String,
}

#[derive(Serialize)]
struct TranslationRequest<'a> {
    q: &'a str,
    source: &'a str,
    target: &'a str,
}

#[derive(Deserialize, Debug)]
struct TranslationResponse {
    #[serde(rename = "translatedText")]
    translated_text: String,
}

/// Sends a chunk of text to the translation API.
async fn translate_chunk(
    client: &reqwest::Client,
    chunk: &str,
    api_url: &str,
    source_lang: &str,
    target_lang: &str,
    bar: &ProgressBar,
) -> Result<String, Box<dyn std::error::Error>> {
    const MAX_RETRIES: u32 = 3;
    let mut last_error: Option<Box<dyn std::error::Error>> = None;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            // Exponential backoff: 1s, 2s, 4s
            let delay = std::time::Duration::from_secs(30 * (1 << attempt));            
            bar.println(format!(
                "Chunk translation failed. Retrying in {:?}... (Attempt {}/{})",
                delay, attempt, MAX_RETRIES
            ));
            tokio::time::sleep(delay).await;
        }

        let request_payload = TranslationRequest {
            q: chunk,
            source: source_lang,
            target: target_lang,
        };

        let response = match client.post(api_url).json(&request_payload).send().await {
            Ok(resp) => resp,
            Err(e) => {
                last_error = Some(e.into());
                continue; // Retry on connection errors
            }
        };

        let status = response.status();
        if status.is_success() {
            let body_text = match response.text().await {
                Ok(text) => text,
                Err(e) => {
                    last_error = Some(e.into());
                    continue; // Retry on error reading body
                }
            };

            match serde_json::from_str::<TranslationResponse>(&body_text) {
                Ok(translation_response) => return Ok(translation_response.translated_text),
                Err(e) => {
                    // JSON decoding error is final, don't retry.
                    let err_msg = format!("Failed to parse JSON from API: {}", e);
                    bar.println(format!("Error: {}", err_msg));
                    bar.println(format!("-- Server Response Body --\n{}\n-- End of Body --", body_text));
                    return Err(err_msg.into());
                }
            }
        } else if status.is_client_error() {
            // 4xx errors are final, don't retry.
            let body_text = response.text().await.unwrap_or_else(|e| format!("Could not read error body: {}", e));
            let err_msg = format!("API request failed with client error status {}", status);
            bar.println(format!("Error: {}", err_msg));
            bar.println(format!("Response body: {}", body_text));
            return Err(err_msg.into());
        } else {
            // 5xx server errors or others, worth retrying.
            let body_text = response.text().await.unwrap_or_else(|e| format!("Could not read error body: {}", e));
            last_error = Some(format!("API request failed with status {}: {}", status, body_text).into());
            // Loop continues to retry
        }
    }

    Err(last_error.unwrap_or_else(|| "Translation failed after multiple retries".into()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // 1. Read the input file
    println!("Reading file: {:?}", args.input_file);
    let content = fs::read_to_string(&args.input_file)?.replace("\r\n", "\n");
    if content.is_empty() {
        println!("Input file is empty. Nothing to translate.");
        return Ok(());
    }

    // 2. Split content into chunks based on paragraphs to respect the API limit
    let paragraphs: Vec<&str> = content.split("\n\n").filter(|p| !p.trim().is_empty()).collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut current_chunk = String::new();

    for paragraph in paragraphs {
        // If a single paragraph is too large, it must be split.
        if paragraph.len() > MAX_CHUNK_SIZE {
            // Push the current chunk if it has anything, before we deal with the big one.
            if !current_chunk.is_empty() {
                chunks.push(current_chunk);
                current_chunk = String::new();
            }

            // Split the large paragraph into smaller pieces.
            let mut remaining = paragraph;
            while !remaining.is_empty() {
                // Find a suitable split point within the size limit.
                let end = if remaining.len() <= MAX_CHUNK_SIZE {
                    remaining.len()
                } else {
                    // Find the last space before the limit to avoid splitting a word.
                    remaining[..MAX_CHUNK_SIZE].rfind(' ').unwrap_or(MAX_CHUNK_SIZE)
                };
                let (piece, rest) = remaining.split_at(end);
                chunks.push(piece.to_string());
                remaining = rest.trim_start();
            }
        } else if current_chunk.len() + paragraph.len() + 2 > MAX_CHUNK_SIZE {
            // The paragraph fits in a chunk by itself, but not in the current one.
            // So, push the current chunk and start a new one.
            chunks.push(current_chunk);
            current_chunk = String::from(paragraph);
        } else {
            // The paragraph fits in the current chunk.
            if !current_chunk.is_empty() {
                current_chunk.push_str("\n\n");
            }
            current_chunk.push_str(paragraph);
        }
    }
    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    println!("Text split into {} chunks for translation.", chunks.len());

    // 3. Translate each chunk
    let client = reqwest::Client::builder()
        .user_agent(format!(
            "rust-text-translator/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let mut translated_chunks = Vec::new();

    let bar = ProgressBar::new(chunks.len() as u64);
    bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")?
            .progress_chars("=>-"),
    );

    for chunk in chunks {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;// Be polite to the public API by waiting a moment between requests (max 8/minute allowed)

        let translated = translate_chunk(
            &client,
            &chunk,
            &args.api_url,
            &args.source,
            &args.target,
            &bar,
        ).await?;
        translated_chunks.push(translated);
        bar.inc(1);
    }

    bar.finish_with_message("Translation complete!");
    let final_translation = translated_chunks.join("\n\n");

    // 4. Output the result
    if let Some(output_path) = args.output_file {
        fs::write(&output_path, final_translation)?;
        println!("Translated text saved to: {:?}", output_path);
    } else {
        println!(
            "\n--- Translated Text ({} -> {}) ---",
            args.source, args.target
        );
        println!("{}", final_translation);
        println!("--- End of Translation ---");
    }

    Ok(())
}