import wave, array, math, sys
def goertzel(x, f, sr):
    k = 2*math.cos(2*math.pi*f/sr); s1=s2=0.0
    for v in x: s0 = v + k*s1 - s2; s2, s1 = s1, s0
    return s1*s1 + s2*s2 - k*s1*s2
for f in sys.argv[1:]:
    w = wave.open(f); ch = w.getnchannels(); sr = w.getframerate()
    a = array.array('h', w.readframes(w.getnframes()))[::ch]  # left channel
    blk = sr // 100
    rms = [math.sqrt(sum(v*v for v in a[i:i+blk])/blk) for i in range(0, len(a)-blk, blk)]
    on = [r for r in rms if r > 300]
    if not on: print(f.split('/')[-1], 'no tone'); continue
    med = sorted(on)[len(on)//2]
    first = next(i for i,r in enumerate(rms) if r > 300); last = len(rms)-1-next(i for i,r in enumerate(reversed(rms)) if r > 300)
    body = rms[first+5:last-5]
    dips = sum(1 for r in body if r < med*0.7)
    seg = a[(first+20)*blk:(first+20)*blk + sr//2]
    tone = goertzel(seg, 1000, sr) * 2 / len(seg)**2
    total = sum(v*v for v in seg)/len(seg)
    clip = sum(1 for v in a if abs(v) >= 32700)
    print(f"{f.split('/')[-1]}: tone {(last-first)/100:.2f}s, level {20*math.log10(med/32768):.1f} dBFS, "
          f"dropout blocks {dips}/{len(body)}, clipped samples {clip}, 1kHz purity {100*tone/total:.1f}%")
