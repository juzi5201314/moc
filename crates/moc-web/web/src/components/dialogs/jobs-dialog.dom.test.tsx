import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { t } from "@/lib/i18n";
import { JobsDialog } from "./jobs-dialog";

afterEach(cleanup);

it("打开尚无任务快照的会话时，后台任务弹窗能呈现空态", () => {
  const view = render(<JobsDialog sessionId="unloaded" open onOpenChange={() => {}} />);
  expect(view.getByRole("dialog", { name: t("job.title") })).toBeTruthy();
  expect(view.getByText(t("job.empty"))).toBeTruthy();
});
