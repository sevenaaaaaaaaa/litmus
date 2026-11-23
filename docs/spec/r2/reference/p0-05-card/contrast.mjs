// oklch → sRGB → WCAG 对比度；零依赖。
// 用途：把「显色状态」是否可读变成数字，而不是感觉。

const clamp01 = (x) => Math.min(1, Math.max(0, x));

export function oklchToSrgb(L, C, hDeg) {
  const h = (hDeg * Math.PI) / 180;
  const a = C * Math.cos(h);
  const b = C * Math.sin(h);
  const l_ = L + 0.3963377774 * a + 0.2158037573 * b;
  const m_ = L - 0.1055613458 * a - 0.0638541728 * b;
  const s_ = L - 0.0894841775 * a - 1.2914855480 * b;
  const l = l_ ** 3, m = m_ ** 3, s = s_ ** 3;
  const lin = {
    r: +4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    g: -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    b: -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
  };
  const enc = (u) => (u <= 0.0031308 ? 12.92 * u : 1.055 * Math.pow(clamp01(u), 1 / 2.4) - 0.055);
  return { r: clamp01(enc(lin.r)), g: clamp01(enc(lin.g)), b: clamp01(enc(lin.b)), lin };
}

export function relLuminance({ lin }) {
  return 0.2126 * clamp01(lin.r) + 0.7152 * clamp01(lin.g) + 0.0722 * clamp01(lin.b);
}

export function contrast(L1, L2) {
  const a = Math.max(L1, L2), b = Math.min(L1, L2);
  return (a + 0.05) / (b + 0.05);
}

export function ratio(fg, bg) {
  return contrast(relLuminance(oklchToSrgb(...fg)), relLuminance(oklchToSrgb(...bg)));
}

export const hex = ([L, C, h]) => {
  const { r, g, b } = oklchToSrgb(L, C, h);
  const to = (v) => Math.round(v * 255).toString(16).padStart(2, '0');
  return '#' + to(r) + to(g) + to(b);
};

// 基线 + 五个显色状态；数值与 tokens.css 保持同步（tests/run.sh 会比对）。
export const palettes = {
  light: {
    paper: [.972, .006, 286],
    sheet: [.992, .003, 286],
    ink: [.255, .022, 282],
    ink3: [.74, .013, 284],
    states: {
      seed: [.62, .012, 284], grounded: [.56, .100, 155], tension: [.62, .150, 72],
      settled: [.53, .190, 259], shipped: [.42, .090, 165],
    },
    stateInk: {
      seed: [.42, .014, 284], grounded: [.42, .085, 155], tension: [.46, .110, 62],
      settled: [.45, .170, 259], shipped: [.36, .080, 165],
    },
  },
  dark: {
    paper: [.185, .014, 278], sheet: [.222, .016, 278], ink: [.93, .010, 282], ink3: [.52, .020, 280],
    states: {
      seed: [.62, .010, 282], grounded: [.72, .090, 155], tension: [.78, .120, 78],
      settled: [.72, .150, 252], shipped: [.74, .080, 165],
    },
    stateInk: {
      seed: [.80, .010, 282], grounded: [.84, .080, 155], tension: [.88, .090, 78],
      settled: [.84, .110, 252], shipped: [.86, .070, 165],
    },
  },
};

export function audit(scheme = 'light') {
  const p = palettes[scheme];
  const rows = [];
  for (const [name, c] of Object.entries(p.states)) {
    rows.push({
      state: name, kind: 'edge', hex: hex(c),
      onPaper: +ratio(c, p.paper).toFixed(2),
      onSheet: +ratio(c, p.sheet).toFixed(2),
      need: 3.0,
    });
    const ink = p.stateInk[name];
    rows.push({
      state: name, kind: 'ink', hex: hex(ink),
      onPaper: +ratio(ink, p.paper).toFixed(2),
      onSheet: +ratio(ink, p.sheet).toFixed(2),
      need: 4.5,
    });
  }
  return rows.map((r) => ({ ...r, pass: r.onPaper >= r.need && r.onSheet >= r.need }));
}
