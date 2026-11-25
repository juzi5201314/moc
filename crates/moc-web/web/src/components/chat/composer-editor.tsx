// 输入框编辑器：CodeMirror 6 承载纯文本，有效引用渲染成整体控件。
//
// 文本模型仍是纯字符串——补全、斜杠命令与发送都按字符串运算；控件只是替换装饰，
// 并登记为原子范围：光标跨过、退格删除都按整体处理。控件的 DOM 槽位由 CodeMirror
// 创建，内容经 React portal 渲染（与对话区共用同一个 RefChip，也能拿到 Tooltip 上下文）。

import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import {
  Annotation,
  Compartment,
  EditorState,
  type Extension,
  Prec,
  StateEffect,
  StateField,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  keymap,
  placeholder as placeholderText,
  WidgetType,
} from "@codemirror/view";
import { type Ref, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { cn } from "@/lib/utils";
import { RefChip, type RefChipData } from "./ref-chip";

export type EditorChip = { from: number; to: number; chip: RefChipData };

export type ComposerEditorHandle = {
  focus: () => void;
  setCaret: (position: number) => void;
  element: () => HTMLElement | null;
  /** 当前编辑器里命令胶囊的真实 DOM 节点（供参数编辑浮层锚定）。 */
  commandChipElement: () => HTMLElement | null;
};

type Props = {
  value: string;
  /** 升序、互不重叠，坐标对应 `value`。 */
  chips: EditorChip[];
  placeholder: string;
  /** 紧跟在当前文本末尾的轻量行内参数占位提示（如 `/ask ` 后的引导文案），不参与文本内容。 */
  ghostHint?: string;
  className?: string;
  minHeight: number;
  maxHeight: number;
  attributes: Record<string, string>;
  /** `typed`：逐字键入；`deleted`：退格/前向删除（用于保持未成形 token 的键入保护）。 */
  onChange: (value: string, cursor: number, typed: boolean, deleted?: boolean) => void;
  onCursor: (cursor: number) => void;
  onBlur?: () => void;
  /** 点击可交互胶囊（如带非自然语言参数的命令胶囊）时触发。 */
  onChipClick?: (chip: EditorChip, element: HTMLElement) => void;
  /** 返回 true 表示已处理，编辑器不再按默认键位响应。 */
  onKeyDown: (event: KeyboardEvent, view: EditorView) => boolean;
  onPaste: (event: ClipboardEvent, view: EditorView) => boolean;
  onCopy: (event: ClipboardEvent, view: EditorView, cut: boolean) => boolean;
  ref?: Ref<ComposerEditorHandle>;
};

/** 控件槽位登记：CodeMirror 建/改/毁 DOM 时通知 React 同步 portal。 */
type SlotHost = {
  mount: (element: HTMLElement, entry: EditorChip) => void;
  update: (element: HTMLElement, entry: EditorChip) => void;
  unmount: (element: HTMLElement) => void;
};

function sameArgs(left?: string[], right?: string[]): boolean {
  if (left === right) return true;
  if (!left || !right || left.length !== right.length) return false;
  return left.every((item, index) => item === right[index]);
}

class ChipWidget extends WidgetType {
  readonly entry: EditorChip;
  readonly host: SlotHost;
  constructor(entry: EditorChip, host: SlotHost) {
    super();
    this.entry = entry;
    this.host = host;
  }
  eq(other: ChipWidget) {
    return (
      other.entry.from === this.entry.from &&
      other.entry.to === this.entry.to &&
      other.entry.chip.kind === this.entry.chip.kind &&
      other.entry.chip.label === this.entry.chip.label &&
      other.entry.chip.detail === this.entry.chip.detail &&
      other.entry.chip.thumb === this.entry.chip.thumb &&
      other.entry.chip.interactive === this.entry.chip.interactive &&
      other.entry.chip.active === this.entry.chip.active &&
      sameArgs(other.entry.chip.args, this.entry.chip.args)
    );
  }
  toDOM() {
    const element = document.createElement("span");
    element.className = "cm-ref-chip";
    element.dataset.chipKind = this.entry.chip.kind;
    this.host.mount(element, this.entry);
    return element;
  }
  /** 同类胶囊原地复用 DOM 节点，避免参数变化时锚定其上的编辑浮层跳动或重建。 */
  updateDOM(element: HTMLElement) {
    if (element.dataset.chipKind !== this.entry.chip.kind) return false;
    this.host.update(element, this.entry);
    return true;
  }
  ignoreEvent(event: Event) {
    if (!this.entry.chip.interactive) return false;
    return (
      event.type === "mousedown" ||
      event.type === "pointerdown" ||
      event.type === "mouseup" ||
      event.type === "pointerup" ||
      event.type === "click"
    );
  }
  destroy(element: HTMLElement) {
    this.host.unmount(element);
  }
}

class GhostHintWidget extends WidgetType {
  readonly text: string;
  constructor(text: string) {
    super();
    this.text = text;
  }
  eq(other: GhostHintWidget) {
    return other.text === this.text;
  }
  toDOM() {
    const span = document.createElement("span");
    span.className = "pointer-events-none select-none text-text-tertiary";
    span.setAttribute("aria-hidden", "true");
    span.dataset.slot = "composer-ghost-hint";
    span.textContent = this.text;
    return span;
  }
  ignoreEvent() {
    return true;
  }
}

const setChips = StateEffect.define<DecorationSet>();
const chipField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, transaction) {
    let next = value.map(transaction.changes);
    for (const effect of transaction.effects) if (effect.is(setChips)) next = effect.value;
    return next;
  },
  provide: (field) => [
    EditorView.decorations.from(field),
    EditorView.atomicRanges.of((view) => view.state.field(field)),
  ],
});

const setGhostHint = StateEffect.define<DecorationSet>();
const ghostHintField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, transaction) {
    let next = value.map(transaction.changes);
    for (const effect of transaction.effects) if (effect.is(setGhostHint)) next = effect.value;
    return next;
  },
  provide: (field) => EditorView.decorations.from(field),
});

/** 外部同步写入的标记：不回调 onChange，避免与受控 value 互相触发。 */
const external = Annotation.define<boolean>();

const placeholderConf = new Compartment();
const attributesConf = new Compartment();
const sizeConf = new Compartment();

const baseTheme = EditorView.theme({
  "&": { color: "inherit", backgroundColor: "transparent", fontSize: "inherit" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "inherit", lineHeight: "inherit", overflowX: "hidden" },
  ".cm-content": { padding: "16px 16px 6px", caretColor: "var(--foreground)" },
  ".cm-line": { padding: "0" },
  "&.cm-focused .cm-cursor": { borderLeftColor: "var(--foreground)" },
  ".cm-placeholder": { color: "var(--text-tertiary)" },
});

function sizeTheme(minHeight: number, maxHeight: number): Extension {
  return EditorView.theme({
    "&": { minHeight: `${minHeight}px` },
    ".cm-scroller": { maxHeight: `${maxHeight}px`, overflowY: "auto" },
  });
}

export function ComposerEditor({
  value,
  chips,
  placeholder,
  ghostHint,
  className,
  minHeight,
  maxHeight,
  attributes,
  onChange,
  onCursor,
  onBlur,
  onChipClick,
  onKeyDown,
  onPaste,
  onCopy,
  ref,
}: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const handlers = useRef({ onChange, onCursor, onBlur, onChipClick, onKeyDown, onPaste, onCopy });
  handlers.current = { onChange, onCursor, onBlur, onChipClick, onKeyDown, onPaste, onCopy };
  const [slots, setSlots] = useState<ReadonlyMap<HTMLElement, EditorChip>>(new Map());
  const slotIds = useRef(new WeakMap<HTMLElement, number>());
  const nextSlot = useRef(0);
  const slotHost = useRef<SlotHost>({
    mount: (element, entry) => {
      slotIds.current.set(element, nextSlot.current++);
      setSlots((current) => new Map(current).set(element, entry));
    },
    update: (element, entry) => {
      setSlots((current) => new Map(current).set(element, entry));
    },
    unmount: (element) =>
      setSlots((current) => {
        const next = new Map(current);
        next.delete(element);
        return next;
      }),
  });

  // 只在挂载时建一次编辑器；可变配置走 Compartment，内容走受控同步。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 初始值只在创建时使用
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: value,
        extensions: [
          history(),
          EditorView.lineWrapping,
          baseTheme,
          chipField,
          ghostHintField,
          placeholderConf.of(placeholderText(placeholder)),
          attributesConf.of(EditorView.contentAttributes.of(attributes)),
          sizeConf.of(sizeTheme(minHeight, maxHeight)),
          Prec.highest(
            EditorView.domEventHandlers({
              keydown: (event, view) =>
                !event.isComposing && !view.composing && handlers.current.onKeyDown(event, view),
              paste: (event, view) => handlers.current.onPaste(event, view),
              copy: (event, view) => handlers.current.onCopy(event, view, false),
              cut: (event, view) => handlers.current.onCopy(event, view, true),
              blur: () => {
                handlers.current.onBlur?.();
                return false;
              },
              // 拖入的文件交给外层拖放区上传；CodeMirror 默认会把文件当文本读进来
              drop: (event) => Boolean(event.dataTransfer?.files.length),
            }),
          ),
          keymap.of([...defaultKeymap, ...historyKeymap]),
          EditorView.updateListener.of((update) => {
            const head = update.state.selection.main.head;
            if (
              update.docChanged &&
              !update.transactions.some((transaction) => transaction.annotation(external))
            ) {
              const typed = update.transactions.some((transaction) =>
                transaction.isUserEvent("input.type"),
              );
              const deleted = update.transactions.some((transaction) =>
                transaction.isUserEvent("delete"),
              );
              handlers.current.onChange(update.state.doc.toString(), head, typed, deleted);
            } else if (update.selectionSet) {
              handlers.current.onCursor(head);
            }
          }),
        ],
      }),
    });
    viewRef.current = view;
    return () => {
      view.destroy();
      viewRef.current = null;
    };
  }, []);

  // 受控内容与控件装饰：只替换差异段，保住光标映射与撤销栈；控件坐标按新文本给出。
  useLayoutEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    const decorations = Decoration.set(
      chips
        .filter((entry) => entry.to <= value.length && entry.from < entry.to)
        .map((entry) =>
          Decoration.replace({ widget: new ChipWidget(entry, slotHost.current) }).range(
            entry.from,
            entry.to,
          ),
        ),
      true,
    );
    const ghostDecorations =
      ghostHint && value.length > 0
        ? Decoration.set([
            Decoration.widget({ widget: new GhostHintWidget(ghostHint), side: 1 }).range(
              value.length,
            ),
          ])
        : Decoration.none;
    if (current === value) {
      view.dispatch({ effects: [setChips.of(decorations), setGhostHint.of(ghostDecorations)] });
      return;
    }
    let start = 0;
    while (start < current.length && start < value.length && current[start] === value[start])
      start++;
    let end = 0;
    while (
      end < current.length - start &&
      end < value.length - start &&
      current[current.length - 1 - end] === value[value.length - 1 - end]
    )
      end++;
    view.dispatch({
      changes: {
        from: start,
        to: current.length - end,
        insert: value.slice(start, value.length - end),
      },
      annotations: external.of(true),
      effects: [setChips.of(decorations), setGhostHint.of(ghostDecorations)],
    });
  }, [value, chips, ghostHint]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: placeholderConf.reconfigure(placeholderText(placeholder)),
    });
  }, [placeholder]);

  const attributesKey = JSON.stringify(attributes);
  // biome-ignore lint/correctness/useExhaustiveDependencies: attributesKey 是 attributes 的内容指纹
  useEffect(() => {
    viewRef.current?.dispatch({
      effects: attributesConf.reconfigure(EditorView.contentAttributes.of(attributes)),
    });
  }, [attributesKey]);

  useEffect(() => {
    viewRef.current?.dispatch({ effects: sizeConf.reconfigure(sizeTheme(minHeight, maxHeight)) });
  }, [minHeight, maxHeight]);

  useImperativeHandle(
    ref,
    () => ({
      focus: () => viewRef.current?.focus(),
      setCaret: (position) => {
        const view = viewRef.current;
        if (!view) return;
        const anchor = Math.min(position, view.state.doc.length);
        view.dispatch({ selection: { anchor }, scrollIntoView: true });
        view.focus();
      },
      element: () => viewRef.current?.dom ?? null,
      commandChipElement: () =>
        hostRef.current?.querySelector<HTMLElement>('.cm-ref-chip[data-chip-kind="command"]') ??
        null,
    }),
    [],
  );

  return (
    <div ref={hostRef} className={cn("w-full", className)}>
      {[...slots].map(([element, entry]) =>
        createPortal(
          <RefChip
            chip={entry.chip}
            onClick={
              entry.chip.interactive
                ? () => handlers.current.onChipClick?.(entry, element)
                : undefined
            }
          />,
          element,
          String(slotIds.current.get(element)),
        ),
      )}
    </div>
  );
}
