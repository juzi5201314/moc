// 用户消息正文：有效引用（`#N` 图片、`#name` 文件、`@` 路径、资源 URI）渲染成可点击引用胶囊，
// 其余原样输出。复制时控件带出原始 token，粘贴回输入框不丢引用。

import { memo, useState } from "react";
import { imageSrc } from "@/components/chat/activity";
import { ImagePreviewDialog } from "@/components/chat/image-preview-dialog";
import { LineBlocks, splitInlineNodesByLine } from "@/components/common/plain-lines";
import { RefChip, type RefChipData } from "@/components/common/ref-chip";
import { isResourceUri } from "@/lib/file-target";
import { useT } from "@/lib/i18n";
import { useRefInfo } from "@/lib/ref-inspect";
import { scanRefs } from "@/lib/refs";
import type { UserAttachment } from "@/lib/transcript-model";
import type { ImageSource } from "@/protocol/types";
import { openFile, revealDir } from "@/store/files";

export const RefText = memo(function RefText({
  sessionId,
  text,
  images,
  attachments,
  className,
}: {
  sessionId: string;
  text: string;
  images: ImageSource[];
  attachments: UserAttachment[];
  className?: string;
}) {
  const t = useT();
  const [preview, setPreview] = useState<{ src: string; title: string } | null>(null);
  const canNavigate = sessionId !== "draft-session";
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
  const refInfo = useRefInfo(canNavigate ? sessionId : null, tokens);
  const out: React.ReactNode[] = [];
  let cursor = 0;
  for (const span of spans) {
    let chip: RefChipData | null | undefined;
    let onClick: (() => void) | undefined;
    if (span.kind === "image") {
      const n = Number(span.token.slice(1));
      const src = imageSrc(images[n - 1]);
      chip = { kind: "image", label: span.token, thumb: src, interactive: true };
      onClick = () => setPreview({ src, title: span.token });
    } else if (span.kind === "file-ref") {
      const file = files.find((entry) => entry.name === span.token.slice(1));
      if (file) {
        const clickable = canNavigate && Boolean(file.path);
        chip = {
          kind: "file-ref",
          label: file.name,
          iconPath: file.path ?? file.name,
          detail: file.path,
          interactive: clickable,
        };
        if (clickable && file.path) {
          const targetPath = file.path;
          onClick = () => openFile(sessionId, { kind: "fs", path: targetPath });
        }
      }
    } else {
      const info = refInfo(span.token);
      if (info) {
        const isAt = span.token.startsWith("@");
        const isRes = !isAt && isResourceUri(span.token);
        const clickable = canNavigate && ((isAt && Boolean(info.detail)) || isRes);
        chip = {
          kind: info.kind,
          label: info.label,
          iconPath: isAt ? (info.detail ?? info.label) : span.token,
          detail: info.detail ?? span.token,
          interactive: clickable,
        };
        if (clickable) {
          if (isRes) {
            const uri = span.token;
            onClick = () => openFile(sessionId, { kind: "resource", uri });
          } else if (info.detail) {
            const targetPath = info.detail;
            onClick =
              info.kind === "dir"
                ? () => revealDir(sessionId, targetPath)
                : () => openFile(sessionId, { kind: "fs", path: targetPath });
          }
        }
      }
    }
    if (!chip) continue;
    out.push(text.slice(cursor, span.from));
    out.push(<RefChip key={span.from} chip={chip} raw={span.token} onClick={onClick} />);
    cursor = span.to;
  }
  out.push(text.slice(cursor));
  return (
    <>
      <LineBlocks lines={splitInlineNodesByLine(out)} className={className} />
      {preview !== null && (
        <ImagePreviewDialog
          open
          src={preview.src}
          alt={t("th.image_preview")}
          title={preview.title}
          onOpenChange={(open) => !open && setPreview(null)}
        />
      )}
    </>
  );
});
