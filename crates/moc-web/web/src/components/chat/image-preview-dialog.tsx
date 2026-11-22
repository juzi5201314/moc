import { useEffect, useState } from "react";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";

type ImagePreviewDialogProps = {
  open: boolean;
  src: string | null;
  alt: string;
  title: string;
  onOpenChange: (open: boolean) => void;
};

function ZoomableImage({ src, alt, open }: { src: string; alt: string; open: boolean }) {
  const [zoom, setZoom] = useState(1);

  useEffect(() => {
    if (open) setZoom(1);
  }, [open]);

  return (
    <img
      src={src}
      alt={alt}
      draggable={false}
      className="max-h-full max-w-full select-none object-contain transition-transform duration-100"
      style={{ transform: `scale(${zoom})` }}
      onWheel={(event) => {
        event.preventDefault();
        setZoom((current) => Math.min(4, Math.max(1, current + (event.deltaY < 0 ? 0.2 : -0.2))));
      }}
    />
  );
}

export function ImagePreviewDialog({
  open,
  src,
  alt,
  title,
  onOpenChange,
}: ImagePreviewDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="w-[min(92vw,1100px)] max-w-none border-0 bg-transparent p-0 shadow-none sm:max-w-none">
        <DialogTitle className="sr-only">{title}</DialogTitle>
        <div className="flex h-[min(86vh,860px)] w-full items-center justify-center overflow-hidden">
          {src && <ZoomableImage key={src} src={src} alt={alt} open={open} />}
        </div>
      </DialogContent>
    </Dialog>
  );
}
