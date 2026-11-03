import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Blueprint } from "./Blueprint.tsx";
import { Btn } from "./Btn.tsx";
import { Tag } from "./Tag.tsx";
import { Seg } from "./Seg.tsx";
import { Bars } from "./Bars.tsx";
import { Check } from "./Check.tsx";

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 16, marginBottom: 16 }}>
      <span style={{ width: 120, fontSize: 12, color: "var(--color-neutral-700)" }}>{label}</span>
      <div style={{ display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap" }}>{children}</div>
    </div>
  );
}

function Gallery() {
  const [seg, setSeg] = useState<"before" | "after">("after");
  return (
    <div style={{ maxWidth: 640 }}>
      <Row label="Blueprint">
        <Blueprint padding="12px 16px">A wireframe object with corner marks</Blueprint>
      </Row>
      <Row label="Btn">
        <Btn variant="primary">Primary</Btn>
        <Btn variant="secondary">Secondary</Btn>
        <Btn variant="ghost">Ghost</Btn>
        <Btn variant="secondary" disabled>
          Disabled
        </Btn>
      </Row>
      <Row label="Tag">
        <Tag variant="accent">Recommended</Tag>
        <Tag variant="outline">Graph</Tag>
        <Tag variant="neutral">Neutral</Tag>
      </Row>
      <Row label="Seg">
        <Seg
          ariaLabel="lane view"
          options={[
            { value: "before", label: "As planned" },
            { value: "after", label: "With plan A" },
          ]}
          value={seg}
          onChange={setSeg}
        />
      </Row>
      <Row label="Bars">
        <Bars value={1} label="risk low" />
        <Bars value={2} label="risk medium" />
        <Bars value={3} label="risk high" />
      </Row>
      <Row label="Check">
        <Check />
      </Row>
    </div>
  );
}

const meta = {
  title: "Primitives/Gallery",
  component: Gallery,
  parameters: { layout: "padded" },
} satisfies Meta<typeof Gallery>;

export default meta;
type Story = StoryObj<typeof meta>;

export const AllStates: Story = {};
