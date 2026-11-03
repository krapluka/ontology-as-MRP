import type { Meta, StoryObj } from "@storybook/react-vite";

// A reference card for the Industry tokens: the accent and neutral ramps and the Barlow /
// Barlow Condensed type scale.

const ACCENT = [100, 200, 300, 400, 500, 600, 700, 800, 900];
const NEUTRAL = [100, 200, 300, 400, 500, 600, 700, 800, 900];

function Ramp({ role, steps }: { role: string; steps: number[] }) {
  return (
    <div style={{ marginBottom: 20 }}>
      <div style={{ fontFamily: "var(--font-heading)", marginBottom: 6 }}>{role}</div>
      <div style={{ display: "flex" }}>
        {steps.map((s) => (
          <div key={s} style={{ flex: 1, textAlign: "center" }}>
            <div style={{ height: 44, background: `var(--color-${role}-${s})` }} />
            <div style={{ fontSize: 10, color: "var(--color-neutral-700)", marginTop: 2 }}>{s}</div>
          </div>
        ))}
      </div>
    </div>
  );
}

function TokenReference() {
  return (
    <div style={{ maxWidth: 720 }}>
      <Ramp role="accent" steps={ACCENT} />
      <Ramp role="neutral" steps={NEUTRAL} />
      <div style={{ display: "flex", gap: 16, marginBottom: 20 }}>
        {(["bg", "surface", "text", "accent"] as const).map((c) => (
          <div key={c}>
            <div
              style={{ width: 64, height: 44, background: `var(--color-${c})`, border: "1px solid var(--color-divider)" }}
            />
            <div style={{ fontSize: 10, marginTop: 2 }}>{c}</div>
          </div>
        ))}
      </div>
      <h1>Barlow Condensed — H1</h1>
      <h2>Barlow Condensed — H2</h2>
      <p style={{ fontFamily: "var(--font-body)" }}>Barlow body 400 — the quick brown fox.</p>
      <p style={{ fontFamily: "var(--font-body)", fontWeight: 500 }}>Barlow body 500 — the quick brown fox.</p>
    </div>
  );
}

const meta = {
  title: "Theme/Tokens",
  component: TokenReference,
  parameters: { layout: "padded" },
} satisfies Meta<typeof TokenReference>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Reference: Story = {};
