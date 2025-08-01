pub fn resample(
    samples: Vec<f32>,
    from: usize,
    to: usize,
) -> Result<Vec<f32>, speexdsp_resampler::Error> {
    // Create resampler
    let mut resampler = speexdsp_resampler::State::new(1, from, to, 4)?;

    // Output buffer
    let mut resampled =
        vec![0.0; ((samples.len() as f64 * to as f64 / from as f64).ceil() as usize) + 512];

    // Downsample
    resampler.process_float(0, &samples, &mut resampled)?;

    Ok(resampled)
}
