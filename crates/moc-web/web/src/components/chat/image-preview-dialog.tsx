// 图片大图预览模态：全视口透明舞台，支持空白处点击关闭、滚轮缩放、拖拽平移与双击复位。

import { XIcon } from "lucide-react";
import { type PointerEvent, useEffect, useRef, useState } from "react";
import { Dialog, DialogClose, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";

const MIN_ZOOM = 1;
const MAX_ZOOM = 4;
const ZOOM_STEP = 0.2;
const DOUBLE_CLICK_ZOOM = 2;

type Offset = { x: number; y: number };
type ViewTransform = { zoom: number; offset: Offset };
type DragState = { pointerId: number; lastX: number; lastY: number };

const ZERO_OFFSET: Offset = { x: 0, y: 0 };
const INITIAL_TRANSFORM: ViewTransform = { zoom: MIN_ZOOM, offset: ZERO_OFFSET };

type ImagePreviewDialogProps = {
  open: boolean;
  src: string | null;
  alt: string;
  title: string;
  onOpenChange: (open: boolean) => void;
};

function clampZoom(value: number): number {
  const rounded = Math.round(value * 10) / 10;
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, rounded));
}

// 图片小于视口时限制在视口内移动，大于视口时贴齐四边，防止把图片拖丢。
function clampAxis(value: number, stageSize: number, baseSize: number, zoom: number): number {
  if (stageSize <= 0 || baseSize <= 0) return value;
  const limit = Math.abs(stageSize - baseSize * zoom) / 2;
  return Math.min(limit, Math.max(-limit, value));
}

function clampOffset(
  offset: Offset,
  stage: HTMLElement | null,
  image: HTMLElement | null,
  zoom: number,
): Offset {
  if (!stage || !image) return offset;
  return {
    x: clampAxis(offset.x, stage.clientWidth, image.offsetWidth, zoom),
    y: clampAxis(offset.y, stage.clientHeight, image.offsetHeight, zoom),
  };
}

function zoomAroundPoint(
  prev: ViewTransform,
  nextZoom: number,
  clientX: number,
  clientY: number,
  stage: HTMLElement | null,
  image: HTMLElement | null,
): ViewTransform {
  if (nextZoom === prev.zoom) return prev;
  if (nextZoom === MIN_ZOOM) return INITIAL_TRANSFORM;

  const ratio = nextZoom / prev.zoom;
  let nextX = prev.offset.x * ratio;
  let nextY = prev.offset.y * ratio;
  if (stage) {
    const rect = stage.getBoundingClientRect();
    if (rect.width > 0 && rect.height > 0) {
      const cx = clientX - (rect.left + rect.width / 2);
      const cy = clientY - (rect.top + rect.height / 2);
      nextX = cx - (cx - prev.offset.x) * ratio;
      nextY = cy - (cy - prev.offset.y) * ratio;
    }
  }
  return {
    zoom: nextZoom,
    offset: clampOffset({ x: nextX, y: nextY }, stage, image, nextZoom),
  };
}

function ImageStage({
  src,
  alt,
  open,
  onClose,
}: {
  src: string | null;
  alt: string;
  open: boolean;
  onClose: () => void;
}) {
  const stageRef = useRef<HTMLDivElement>(null);
  const imgRef = useRef<HTMLImageElement>(null);
  const dragRef = useRef<DragState | null>(null);
  const suppressBackdropClickRef = useRef(false);
  const [transform, setTransform] = useState<ViewTransform>(INITIAL_TRANSFORM);
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    if (open) {
      setTransform(INITIAL_TRANSFORM);
      dragRef.current = null;
      suppressBackdropClickRef.current = false;
      setDragging(false);
    }
  }, [open]);

  useEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const handleWheel = (event: WheelEvent) => {
      event.preventDefault();
      if (event.deltaY === 0) return;
      const delta = event.deltaY < 0 ? ZOOM_STEP : -ZOOM_STEP;
      setTransform((prev) =>
        zoomAroundPoint(
          prev,
          clampZoom(prev.zoom + delta),
          event.clientX,
          event.clientY,
          stageRef.current,
          imgRef.current,
        ),
      );
    };
    stage.addEventListener("wheel", handleWheel, { passive: false });
    return () => stage.removeEventListener("wheel", handleWheel);
  }, []);

  const handleImagePointerDown = (event: PointerEvent<HTMLImageElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture?.(event.pointerId);
    dragRef.current = {
      pointerId: event.pointerId,
      lastX: event.clientX,
      lastY: event.clientY,
    };
    setDragging(true);
  };

  const handleImagePointerMove = (event: PointerEvent<HTMLImageElement>) => {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) return;
    const dx = event.clientX - drag.lastX;
    const dy = event.clientY - drag.lastY;
    drag.lastX = event.clientX;
    drag.lastY = event.clientY;
    setTransform((prev) => ({
      zoom: prev.zoom,
      offset: clampOffset(
        { x: prev.offset.x + dx, y: prev.offset.y + dy },
        stageRef.current,
        imgRef.current,
        prev.zoom,
      ),
    }));
  };

  const endDrag = (event: PointerEvent<HTMLImageElement>) => {
    if (dragRef.current?.pointerId !== event.pointerId) return;
    dragRef.current = null;
    setDragging(false);
    if (event.currentTarget.hasPointerCapture?.(event.pointerId)) {
      event.currentTarget.releasePointerCapture?.(event.pointerId);
    }
  };

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: 点击全屏空白舞台关闭，键盘用户走 Esc 或关闭按钮
    // biome-ignore lint/a11y/useKeyWithClickEvents: 键盘关闭由 Radix Dialog 的 Esc 与 DialogClose 负责
    <div
      ref={stageRef}
      data-slot="image-preview-stage"
      className="flex size-full items-center justify-center overflow-hidden"
      onPointerDown={(event) => {
        // 指针起手在图片上时，拖拽结束即使 click 落在空白处也不误触关闭。
        suppressBackdropClickRef.current = event.target !== event.currentTarget;
      }}
      onClick={(event) => {
        const suppressed = suppressBackdropClickRef.current;
        suppressBackdropClickRef.current = false;
        if (!suppressed && event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      {src && (
        <img
          ref={imgRef}
          src={src}
          alt={alt}
          draggable={false}
          className={cn(
            "max-h-[86dvh] max-w-[92dvw] touch-none select-none object-contain",
            dragging ? "cursor-grabbing" : "cursor-grab transition-transform duration-100",
          )}
          style={{
            transform: `translate3d(${transform.offset.x}px, ${transform.offset.y}px, 0) scale(${transform.zoom})`,
          }}
          onPointerDown={handleImagePointerDown}
          onPointerMove={handleImagePointerMove}
          onPointerUp={endDrag}
          onPointerCancel={endDrag}
          onLostPointerCapture={endDrag}
          onDoubleClick={(event) => {
            setTransform((prev) => {
              if (prev.zoom > MIN_ZOOM || prev.offset.x !== 0 || prev.offset.y !== 0) {
                return INITIAL_TRANSFORM;
              }
              return zoomAroundPoint(
                prev,
                DOUBLE_CLICK_ZOOM,
                event.clientX,
                event.clientY,
                stageRef.current,
                imgRef.current,
              );
            });
          }}
        />
      )}
    </div>
  );
}

export function ImagePreviewDialog({
  open,
  src,
  alt,
  title,
  onOpenChange,
}: ImagePreviewDialogProps) {
  const t = useT();

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="inset-0 block h-dvh w-dvw max-w-none translate-x-0 translate-y-0 gap-0 rounded-none border-0 bg-transparent p-0 shadow-none sm:max-w-none"
      >
        <DialogTitle className="sr-only">{title}</DialogTitle>
        <DialogClose
          aria-label={t("common.close")}
          className="absolute top-4 right-4 z-10 flex size-9 items-center justify-center rounded-full bg-popover/90 text-foreground shadow-[var(--elevation-popover)] transition-colors hover:bg-popover focus-visible:outline-2 [&_svg]:size-4"
        >
          <XIcon />
        </DialogClose>
        <ImageStage
          key={src ?? ""}
          src={src}
          alt={alt}
          open={open}
          onClose={() => onOpenChange(false)}
        />
      </DialogContent>
    </Dialog>
  );
}
