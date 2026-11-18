// CamillaDSP - A flexible tool for processing audio
// Copyright (C) 2026 Henrik Enquist
//
// This file is part of CamillaDSP.
//
// CamillaDSP is free software; you can redistribute it and/or modify it
// under the terms of either:
//
// a) the GNU General Public License version 3,
//    or
// b) the Mozilla Public License Version 2.0.
//
// You should have received copies of the GNU General Public License and the
// Mozilla Public License along with this program. If not, see
// <https://www.gnu.org/licenses/> and <https://www.mozilla.org/MPL/2.0/>.

use crate::PrcFmt;
use crate::Res;
use crate::audiochunk::AudioChunk;
use crate::config;
use crate::utils::decibels::gain_from_value;
use crate::utils::stash::{container_from_stash, recycle_chunk, vec_from_stash};

#[derive(Clone)]
pub struct Mixer {
    pub name: String,
    pub channels_in: usize,
    pub channels_out: usize,
    pub mapping: Vec<Vec<MixerSource>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MixerSource {
    pub channel: usize,
    pub gain: PrcFmt,
}

impl Mixer {
    /// Creates a Mixer from a config struct
    pub fn from_config(name: String, config: config::Mixer) -> Self {
        let ch_in = config.channels.r#in;
        let ch_out = config.channels.out;
        let mut mapping = vec![Vec::<MixerSource>::new(); ch_out];
        for cfg_mapping in config.mapping {
            if !cfg_mapping.is_mute() {
                let dest = cfg_mapping.dest;
                for cfg_src in cfg_mapping.sources {
                    if !cfg_src.is_mute() {
                        let gain_value = cfg_src.gain();
                        let inverted = cfg_src.is_inverted();
                        let linear = cfg_src.scale() == config::GainScale::Linear;
                        let gain = gain_from_value(gain_value, linear, inverted, false);
                        let src = MixerSource {
                            channel: cfg_src.channel,
                            gain,
                        };
                        mapping[dest].push(src);
                    }
                }
            }
        }
        Mixer {
            name,
            channels_in: ch_in,
            channels_out: ch_out,
            mapping,
        }
    }

    pub fn update_parameters(&mut self, config: config::Mixer) {
        let ch_in = config.channels.r#in;
        let ch_out = config.channels.out;
        let mut mapping = vec![Vec::<MixerSource>::new(); ch_out];
        for cfg_mapping in config.mapping {
            let dest = cfg_mapping.dest;
            for cfg_src in cfg_mapping.sources {
                let gain_value = cfg_src.gain();
                let inverted = cfg_src.is_inverted();
                let linear = cfg_src.scale() == config::GainScale::Linear;
                let gain = gain_from_value(gain_value, linear, inverted, false);
                let src = MixerSource {
                    channel: cfg_src.channel,
                    gain,
                };
                mapping[dest].push(src);
            }
        }
        self.channels_in = ch_in;
        self.channels_out = ch_out;
        self.mapping = mapping;
    }

    /// Apply a Mixer to an AudioChunk, yielding a new AudioChunk with a possibly different number of channels.
    pub fn process_chunk(&mut self, input: AudioChunk) -> AudioChunk {
        let mut waveforms = container_from_stash(self.channels_out);
        for out_chan in 0..self.channels_out {
            let v = vec_from_stash(input.frames);
            waveforms.push(v);
            for source in 0..self.mapping[out_chan].len() {
                let source_chan = self.mapping[out_chan][source].channel;
                if !input.waveforms[source_chan].is_empty() {
                    let gain = self.mapping[out_chan][source].gain;
                    for (out_val, in_val) in waveforms[out_chan]
                        .iter_mut()
                        .zip(input.waveforms[source_chan].iter())
                        .take(input.frames)
                    {
                        *out_val += gain * in_val;
                    }
                }
            }
        }

        let new_chunk = AudioChunk::from(&input, waveforms);
        recycle_chunk(input);
        new_chunk
    }
}

/// Validate the mixer config, to give a helpful message intead of a panic.
pub fn validate_mixer(mixer_config: &config::Mixer) -> Res<()> {
    let chan_in = mixer_config.channels.r#in;
    let chan_out = mixer_config.channels.out;
    let mut output_channels: Vec<usize> = Vec::with_capacity(chan_out);
    let mut input_channels: Vec<usize> = Vec::with_capacity(chan_in);
    for mapping in mixer_config.mapping.iter() {
        if mapping.dest >= chan_out {
            let msg = format!(
                "Invalid destination channel {}, max is {}.",
                mapping.dest,
                chan_out - 1
            );
            return Err(config::ConfigError::new(&msg).into());
        }
        if output_channels.contains(&mapping.dest) {
            let msg = format!(
                "There is more than one mapping for destination channel {}",
                mapping.dest,
            );
            return Err(config::ConfigError::new(&msg).into());
        }
        output_channels.push(mapping.dest);
        input_channels.clear();
        for source in mapping.sources.iter() {
            if source.channel >= chan_in {
                let msg = format!(
                    "Invalid source channel {}, max is {}.",
                    source.channel,
                    chan_in - 1
                );
                return Err(config::ConfigError::new(&msg).into());
            }
            if input_channels.contains(&source.channel) {
                let msg = format!(
                    "Input channel {} is listed mote than once for destination channel {}",
                    source.channel, mapping.dest,
                );
                return Err(config::ConfigError::new(&msg).into());
            }
        }
    }
    Ok(())
}

/// Get a vector showing which input channels are used
pub fn used_input_channels(mixer_config: &config::Mixer) -> Vec<bool> {
    let chan_in = mixer_config.channels.r#in;
    let mut used_channels = vec![false; chan_in];
    for mapping in mixer_config.mapping.iter() {
        if !mapping.is_mute() {
            for source in mapping.sources.iter() {
                if !source.is_mute() {
                    used_channels[source.channel] = true;
                }
            }
        }
    }
    used_channels
}

#[cfg(test)]
mod tests {
    use crate::config::{Mixer, MixerChannels, MixerMapping, MixerSource};
    use crate::mixer;
    use crate::mixer::used_input_channels;

    #[test]
    fn sparse_mixer_preserves_upstream_compressor_sidechain() {
        use crate::config::{self, BinarySampleFormat};
        use crate::pipeline::Pipeline;
        use crate::utils::conversions::buffer_to_chunk_rawbytes;
        use serde_json::json;

        let mut raw = serde_json::to_value(crate::protection::tests::configuration()).unwrap();
        raw["devices"]["capture"] = json!({
            "type": "Alsa", "device": "hw:test", "channels": 4, "format": "S16_LE"
        });
        // RACE feeds input 2 into the compressor's monitor on 1. Input 3 is unused.
        raw["processors"]["crossfeed"] = json!({"type": "RACE", "parameters": {
            "channels": 4, "channel_a": 1, "channel_b": 2, "delay": 0.15, "attenuation": 6
        }});
        raw["processors"]["compressor"] = json!({"type": "Compressor", "parameters": {
            "channels": 4, "monitor_channels": [1], "process_channels": [0],
            "attack": 0.0005, "release": 0.1, "threshold": -20, "factor": 4
        }});
        raw["mixers"] = json!({"route": {"channels": {"in": 4, "out": 24},
            "mapping": [{"dest": 0, "sources": [{"channel": 0}]}]}});
        raw["pipeline"] = json!([
            {"type": "Processor", "name": "crossfeed"},
            {"type": "Processor", "name": "compressor"},
            {"type": "Mixer", "name": "route"},
            {"type": "Processor", "name": "guard"}
        ]);
        let conf = config::validate_config(serde_json::from_value(raw).unwrap(), None).unwrap();
        let frames = conf.config().devices.chunksize;
        let used = config::used_capture_channels(conf.config());
        let mut pipeline = Pipeline::from_config(conf.into_config(), Default::default());
        let mut routed_sample = |sidechain: i16| {
            let frame = [4096, 0, sidechain, i16::MAX]
                .map(i16::to_le_bytes)
                .concat();
            let buffer = frame.repeat(frames);
            let chunk = buffer_to_chunk_rawbytes(
                &buffer,
                4,
                &BinarySampleFormat::S16_LE,
                buffer.len(),
                &used,
                false,
            );
            assert!(chunk.waveforms[3].is_empty());
            pipeline.process_chunk(chunk).waveforms[0][frames - 1]
        };
        let quiet = routed_sample(0);
        let loud = routed_sample(24576);
        assert!((quiet - 0.125).abs() < 1e-6);
        assert!(loud > 0.0 && loud < quiet / 2.0);
    }

    #[test]
    fn check_all_used() {
        let chans = MixerChannels { r#in: 2, out: 4 };
        let src0 = MixerSource {
            channel: 0,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src1 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src2 = MixerSource {
            channel: 0,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src3 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let map0 = MixerMapping {
            dest: 0,
            sources: vec![src0],
            mute: Some(false),
        };
        let map1 = MixerMapping {
            dest: 1,
            sources: vec![src1],
            mute: Some(false),
        };
        let map2 = MixerMapping {
            dest: 2,
            sources: vec![src2],
            mute: Some(false),
        };
        let map3 = MixerMapping {
            dest: 3,
            sources: vec![src3],
            mute: Some(false),
        };
        let conf = Mixer {
            description: None,
            channels: chans,
            mapping: vec![map0, map1, map2, map3],
            labels: None,
        };
        let used = used_input_channels(&conf);
        assert_eq!(used, vec![true, true]);
    }

    #[test]
    fn check_not_mapped() {
        let chans = MixerChannels { r#in: 2, out: 4 };
        let src0 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src1 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src2 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src3 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let map0 = MixerMapping {
            dest: 0,
            sources: vec![src0],
            mute: Some(false),
        };
        let map1 = MixerMapping {
            dest: 1,
            sources: vec![src1],
            mute: Some(false),
        };
        let map2 = MixerMapping {
            dest: 2,
            sources: vec![src2],
            mute: Some(false),
        };
        let map3 = MixerMapping {
            dest: 3,
            sources: vec![src3],
            mute: Some(false),
        };
        let conf = Mixer {
            description: None,
            channels: chans,
            mapping: vec![map0, map1, map2, map3],
            labels: None,
        };
        let used = used_input_channels(&conf);
        assert_eq!(used, vec![false, true]);
    }

    #[test]
    fn check_mute_source() {
        let chans = MixerChannels { r#in: 2, out: 4 };
        let src0 = MixerSource {
            channel: 0,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(true),
            scale: None,
        };
        let src1 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src2 = MixerSource {
            channel: 0,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(true),
            scale: None,
        };
        let src3 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let map0 = MixerMapping {
            dest: 0,
            sources: vec![src0],
            mute: Some(false),
        };
        let map1 = MixerMapping {
            dest: 1,
            sources: vec![src1],
            mute: Some(false),
        };
        let map2 = MixerMapping {
            dest: 2,
            sources: vec![src2],
            mute: Some(false),
        };
        let map3 = MixerMapping {
            dest: 3,
            sources: vec![src3],
            mute: Some(false),
        };
        let conf = Mixer {
            description: None,
            channels: chans,
            mapping: vec![map0, map1, map2, map3],
            labels: None,
        };
        let used = used_input_channels(&conf);
        assert_eq!(used, vec![false, true]);
    }

    #[test]
    fn check_mute_mapping() {
        let chans = MixerChannels { r#in: 2, out: 4 };
        let src0 = MixerSource {
            channel: 0,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src1 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src2 = MixerSource {
            channel: 0,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src3 = MixerSource {
            channel: 1,
            gain: Some(-3.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let map0 = MixerMapping {
            dest: 0,
            sources: vec![src0],
            mute: Some(true),
        };
        let map1 = MixerMapping {
            dest: 1,
            sources: vec![src1],
            mute: Some(false),
        };
        let map2 = MixerMapping {
            dest: 2,
            sources: vec![src2],
            mute: Some(true),
        };
        let map3 = MixerMapping {
            dest: 3,
            sources: vec![src3],
            mute: Some(false),
        };
        let conf = Mixer {
            description: None,
            channels: chans,
            mapping: vec![map0, map1, map2, map3],
            labels: None,
        };
        let used = used_input_channels(&conf);
        assert_eq!(used, vec![false, true]);
    }

    #[test]
    fn check_make_mixer() {
        let chans = MixerChannels { r#in: 2, out: 4 };
        let src0 = MixerSource {
            channel: 0,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src1 = MixerSource {
            channel: 1,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src2 = MixerSource {
            channel: 0,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src3 = MixerSource {
            channel: 1,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let map0 = MixerMapping {
            dest: 0,
            sources: vec![src0],
            mute: Some(false),
        };
        let map1 = MixerMapping {
            dest: 1,
            sources: vec![src1],
            mute: Some(false),
        };
        let map2 = MixerMapping {
            dest: 2,
            sources: vec![src2],
            mute: Some(false),
        };
        let map3 = MixerMapping {
            dest: 3,
            sources: vec![src3],
            mute: Some(false),
        };
        let conf = Mixer {
            description: None,
            channels: chans,
            mapping: vec![map0, map1, map2, map3],
            labels: None,
        };
        let mix = mixer::Mixer::from_config("dummy".to_string(), conf);
        assert_eq!(mix.channels_in, 2);
        assert_eq!(mix.channels_out, 4);

        let exp_src0 = mixer::MixerSource {
            channel: 0,
            gain: 1.0,
        };
        let exp_src1 = mixer::MixerSource {
            channel: 1,
            gain: 1.0,
        };
        let exp_src2 = mixer::MixerSource {
            channel: 0,
            gain: 1.0,
        };
        let exp_src3 = mixer::MixerSource {
            channel: 1,
            gain: 1.0,
        };

        let exp_map = vec![
            vec![exp_src0],
            vec![exp_src1],
            vec![exp_src2],
            vec![exp_src3],
        ];

        assert_eq!(mix.mapping, exp_map);
    }

    #[test]
    fn check_make_mixer_muted() {
        let chans = MixerChannels { r#in: 2, out: 4 };
        let src0 = MixerSource {
            channel: 0,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src1 = MixerSource {
            channel: 1,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src2 = MixerSource {
            channel: 0,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let src3 = MixerSource {
            channel: 1,
            gain: Some(0.0),
            inverted: Some(false),
            mute: Some(false),
            scale: None,
        };
        let map0 = MixerMapping {
            dest: 0,
            sources: vec![src0],
            mute: Some(true),
        };
        let map1 = MixerMapping {
            dest: 1,
            sources: vec![src1],
            mute: Some(false),
        };
        let map2 = MixerMapping {
            dest: 2,
            sources: vec![src2],
            mute: Some(true),
        };
        let map3 = MixerMapping {
            dest: 3,
            sources: vec![src3],
            mute: Some(false),
        };
        let conf = Mixer {
            description: None,
            channels: chans,
            mapping: vec![map0, map1, map2, map3],
            labels: None,
        };
        let mix = mixer::Mixer::from_config("dummy".to_string(), conf);
        assert_eq!(mix.channels_in, 2);
        assert_eq!(mix.channels_out, 4);

        //let exp_src0 = mixer::MixerSource {channel: 0, gain: 1.0};
        let exp_src1 = mixer::MixerSource {
            channel: 1,
            gain: 1.0,
        };
        //let exp_src2 = mixer::MixerSource {channel: 0, gain: 1.0};
        let exp_src3 = mixer::MixerSource {
            channel: 1,
            gain: 1.0,
        };

        let exp_map = vec![vec![], vec![exp_src1], vec![], vec![exp_src3]];

        assert_eq!(mix.mapping, exp_map);
    }
}
