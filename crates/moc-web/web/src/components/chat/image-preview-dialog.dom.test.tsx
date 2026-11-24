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

function defineSizes(el: HTMLElement, sizes: Record<string, number>) {
  for (const [key, value] of Object.entries(sizes)) {
    Object.defineProperty(el, key, { value, configurable: true });
  }
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

it("closes on blank backdrop click but stays open when clicking or dragging the image", () => {
  const view = render(<PreviewHarness src="first.png" />);
  const image = view.getByAltText("Preview");
  const stage = image.parentElement as HTMLElement;

  // 直接点击图片不关闭。
  fireEvent.pointerDown(image, { pointerId: 1, button: 0, clientX: 100, clientY: 100 });
  fireEvent.pointerUp(image, { pointerId: 1, button: 0, clientX: 100, clientY: 100 });
  fireEvent.click(image);
  expect(view.queryByAltText("Preview")).not.toBeNull();

  // 在图片上拖拽并模拟松手后 click 落在空白舞台，不应误关预览。
  fireEvent.pointerDown(image, { pointerId: 1, button: 0, clientX: 100, clientY: 100 });
  fireEvent.pointerMove(image, { pointerId: 1, clientX: 140, clientY: 75 });
  fireEvent.pointerUp(image, { pointerId: 1, button: 0, clientX: 140, clientY: 75 });
  fireEvent.click(stage);
  expect(view.queryByAltText("Preview")).not.toBeNull();
  expect(image.getAttribute("style")).toContain("translate3d(40px, -25px, 0)");

  // 点击图片外空白舞台直接关闭预览。
  fireEvent.pointerDown(stage, { pointerId: 2, button: 0, clientX: 10, clientY: 10 });
  fireEvent.click(stage);
  expect(view.queryByAltText("Preview")).toBeNull();
});

it("clamps pan within stage bounds and resets offset on double-click or zoom-out", () => {
  const view = render(<PreviewHarness src="first.png" />);
  const image = view.getByAltText("Preview");
  const stage = image.parentElement as HTMLElement;

  // 小图（400×300）放在 1000×800 舞台里，1x 最大可拖偏移为 (300, 250)。
  defineSizes(stage, { clientWidth: 1000, clientHeight: 800 });
  defineSizes(image, { offsetWidth: 400, offsetHeight: 300 });

  fireEvent.pointerDown(image, { pointerId: 1, button: 0, clientX: 500, clientY: 400 });
  fireEvent.pointerMove(image, { pointerId: 1, clientX: 1000, clientY: 0 });
  fireEvent.pointerUp(image, { pointerId: 1, button: 0, clientX: 1000, clientY: 0 });
  expect(image.getAttribute("style")).toContain("translate3d(300px, -250px, 0) scale(1)");

  // 双击有偏移的图直接复位，再双击放大到 2x。
  fireEvent.doubleClick(image);
  expect(image.getAttribute("style")).toContain("translate3d(0px, 0px, 0) scale(1)");

  fireEvent.doubleClick(image);
  expect(image.getAttribute("style")).toContain("scale(2)");

  // 放大后拖动再滚轮缩回 1x 时，偏移一并归零。
  fireEvent.pointerDown(image, { pointerId: 2, button: 0, clientX: 500, clientY: 400 });
  fireEvent.pointerMove(image, { pointerId: 2, clientX: 560, clientY: 430 });
  fireEvent.pointerUp(image, { pointerId: 2, button: 0, clientX: 560, clientY: 430 });
  expect(image.getAttribute("style")).toContain("translate3d(60px, 30px, 0) scale(2)");

  for (let i = 0; i < 5; i += 1) {
    fireEvent.wheel(stage, { deltaY: 100 });
  }
  expect(image.getAttribute("style")).toContain("translate3d(0px, 0px, 0) scale(1)");
});
