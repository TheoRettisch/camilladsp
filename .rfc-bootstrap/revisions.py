#!/usr/bin/env python3
"""Follow-up corrections identified by the first Rust CI run.
Applies only to the exact unformatted source produced by bundle.py.
"""
from pathlib import Path
import sys

root = Path(sys.argv[1])
def replace(path: str, old: str, new: str) -> None:
    target = root / path
    text = target.read_text()
    if text.count(old) != 1:
        raise RuntimeError(f'Expected one revision anchor in {path}')
    target.write_text(text.replace(old, new))

replace('src/filters/lookahead_limiter.rs',
    '            config::CompressorParameters {\n                channels: 1,\n                monitor_channels: None,\n',
    '            config::CompressorParameters {\n                channels: 1,\n                monitor_channels: None,\n                monitor_mode: None,\n')
replace('benches/monitor_modes.rs',
    '#[path = "../src/processors/monitor.rs"]\nmod monitor;',
    '// Criterion does not execute the imported module\'s unit-test helpers.\n#[allow(dead_code)]\n#[path = "../src/processors/monitor.rs"]\nmod monitor;')
replace('tests/monitor_modes.rs',
    '''                    for i in 0..size {
                        let time = start + i;
                        if time < 128 {
                            samples[2][i] = (time as CamillaFloat * 0.31).sin() * 4.0;
                            samples[3][i] = (time as CamillaFloat * 0.17).cos() * 2.0;
                        }
                    }''',
    '''                    let (first, second) = samples[2..].split_at_mut(1);
                    for (i, (left, right)) in first[0].iter_mut().zip(&mut second[0]).enumerate() {
                        let time = start + i;
                        if time < 128 {
                            *left = (time as CamillaFloat * 0.31).sin() * 4.0;
                            *right = (time as CamillaFloat * 0.17).cos() * 2.0;
                        }
                    }''')
replace('tests/monitor_modes.rs',
    '''                for i in 0..8 {
                    waveforms[0][i] = if (block + i) % 2 == 0 { 4.0 } else { -2.0 };
                    waveforms[1][i] = if (block + i) % 2 == 0 { -2.0 } else { 4.0 };
                }''',
    '''                let (first, second) = waveforms.split_at_mut(1);
                for (i, (left, right)) in first[0].iter_mut().zip(&mut second[0]).enumerate() {
                    *left = if (block + i) % 2 == 0 { 4.0 } else { -2.0 };
                    *right = if (block + i) % 2 == 0 { -2.0 } else { 4.0 };
                }''')
print('Applied compiler-initializer correction and three scoped lint cleanups.')
