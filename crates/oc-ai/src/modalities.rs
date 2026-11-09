//! Non-chat modalities (spec §8). Image generation ([`crate::image`]) and decisions ([`crate::decision`]) are ported
//! for their inline `generate` call, the one core makes.
//!
//! TODO: the image stream and queued generations (`Image.stream`, `start`, `resume`), video (`google-video`,
//! `xai-video`, `fal-video`, `runway-video`), speech (`openai-speech`, `google-speech`, `elevenlabs-speech`,
//! `deepgram-speech`, `cartesia-speech`), transcription (`openai-transcription`, `google-transcription`,
//! `elevenlabs-transcription`, `deepgram-transcription`, `assemblyai-transcription`), the polling `Generation`
//! helper and the experimental evaluation clients. None are used by core.
