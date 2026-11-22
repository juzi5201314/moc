import { fireEvent, render } from "@testing-library/react";
import { useState } from "react";
import { expect, it } from "vitest";
import { ImagePreviewDialog } from "./image-preview-dialog";

function PreviewHarness({ src }: { src: string }) {
  const [open, setOpen] = useState(true);
  return (
    <ImagePreviewDialog
      open={open}
      src={src}
      alt="Preview"
      title="Image preview"
      onOpenChange={setOpen}
    />
  );
}

it("zooms with the wheel, clamps at the minimum, and resets when the image changes", () => {
  const view = render(<PreviewHarness src="first.png" />);
  const image = view.getByAltText("Preview");

  fireEvent.wheel(image, { deltaY: -100 });
  expect(image.getAttribute("style")).toContain("scale(1.2)");

  fireEvent.wheel(image, { deltaY: 100 });
  fireEvent.wheel(image, { deltaY: 100 });
  expect(image.getAttribute("style")).toContain("scale(1)");

  view.rerender(<PreviewHarness src="second.png" />);
  expect(view.getByAltText("Preview").getAttribute("style")).toContain("scale(1)");
});
