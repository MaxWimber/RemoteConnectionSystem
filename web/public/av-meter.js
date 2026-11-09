// AudioWorklet of the sound and picture measurement (P9-1): reports the context time of every click, a sample
// louder than the threshold after at least 300 ms of quiet.
class ClickMeter extends AudioWorkletProcessor {
  constructor() {
    super();
    this.threshold = 0.2;
    this.quiet = 0.3 * sampleRate;
    this.sinceLoud = this.quiet;
  }

  process(inputs) {
    const input = inputs[0];
    const left = input && input[0];
    if (!left) return true;
    const right = input[1] || left;
    for (let i = 0; i < left.length; i += 1) {
      const v = Math.max(Math.abs(left[i]), Math.abs(right[i]));
      if (v > this.threshold) {
        if (this.sinceLoud >= this.quiet) this.port.postMessage((currentFrame + i) / sampleRate);
        this.sinceLoud = 0;
      } else {
        this.sinceLoud += 1;
      }
    }
    return true;
  }
}

registerProcessor('click-meter', ClickMeter);
