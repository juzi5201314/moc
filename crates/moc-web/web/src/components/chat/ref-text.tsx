// 用户消息正文：有效引用（`#N` 图片、`#name` 文件、`@` 路径、资源 URI）渲染成引用控件，
// 其余原样输出。复制时控件带出原始 token，粘贴回输入框不丢引用。

import { memo } from "react";
import { imageSrc } from "@/components/chat/activity";
import { useRefInfo } from "@/lib/ref-inspect";
import { scanRefs } from "@/lib/refs";
import type { UserAttachment } from "@/lib/transcript-model";
import type { ImageSource } from "@/protocol/types";
import { RefChip, type RefChipData } from "./ref-chip";

export const RefText = memo(function RefText({
  sessionId,
  text,
  images,
  attachments,
}: {
  sessionId: string;
  text: string;
  images: ImageSource[];
  attachments: UserAttachment[];
}) {
  const files = attachments.flatMap((attachment) =>
    attachment.ref ? [{ name: attachment.ref.slice(1), path: attachment.path }] : [],
  );
  const spans = scanRefs(text, {
    imageCount: images.length,
    fileNames: files.map((file) => file.name),
  });
  const tokens = [
    ...new Set(spans.filter((span) => span.kind === "resource").map((span) => span.token)),
  ];
  const refInfo = useRefInfo(sessionId === "draft-session" ? null : sessionId, tokens);
  const out: React.ReactNode[] = [];
  let cursor = 0;
  for (const span of spans) {
    let chip: RefChipData | null | undefined;
    if (span.kind === "image") {
      const n = Number(span.token.slice(1));
      chip = { kind: "image", label: span.token, thumb: imageSrc(images[n - 1]) };
    } else if (span.kind === "file-ref") {
      const file = files.find((entry) => entry.name === span.token.slice(1));
      chip = file && { kind: "file-ref", label: file.name, detail: file.path };
    } else {
      chip = refInfo(span.token);
    }
    if (!chip) continue;
    out.push(text.slice(cursor, span.from));
    out.push(<RefChip key={span.from} chip={chip} raw={span.token} />);
    cursor = span.to;
  }
  out.push(text.slice(cursor));
  return <>{out}</>;
});
