//! Non-chat modalities (spec §8). Image generation ([`crate::image`]) and decisions ([`crate::decision`]) are ported
//! for their inline `generate` call, the one core makes; video ([`crate::video`], 2.4.1) for the queued `start`,
//! `status`, `result` and `cancel` calls core's video jobs make, with every video protocol a bundled package uses
//! (`xai-video`, `minimax-video`, `minimax-hailuo-video`, `openrouter-video`, `seedance-video`,
//! `google-interactions-video`).
//!
//! TODO: the image stream and queued image generations (`Image.stream`, `start`, `resume`), the `Generation` handle's
//! `await`/`events` polling for video, speech (`openai-speech`, `google-speech`, `elevenlabs-speech`,
//! `deepgram-speech`, `cartesia-speech`), transcription (`openai-transcription`, `google-transcription`,
//! `elevenlabs-transcription`, `deepgram-transcription`, `assemblyai-transcription`), the polling `Generation`
//! helper and the experimental evaluation clients. None are used by core.
