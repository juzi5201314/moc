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
};

type Props = {
  value: string;
  /** 升序、互不重叠，坐标对应 `value`。 */
  chips: EditorChip[];
  placeholder: string;
  className?: string;
  minHeight: number;
  maxHeight: number;
  attributes: Record<string, string>;
  /** `typed`：这次变化来自逐字键入（引用此时还在输入中，不应成形）。 */
  onChange: (value: string, cursor: number, typed: boolean) => void;
  onCursor: (cursor: number) => void;
  /** 返回 true 表示已处理，编辑器不再按默认键位响应。 */
  onKeyDown: (event: KeyboardEvent, view: EditorView) => boolean;
  onPaste: (event: ClipboardEvent, view: EditorView) => boolean;
  onCopy: (event: ClipboardEvent, view: EditorView, cut: boolean) => boolean;
  ref?: Ref<ComposerEditorHandle>;
};

/** 控件槽位登记：CodeMirror 建/毁 DOM 时通知 React 增删 portal。 */
type SlotHost = {
  mount: (element: HTMLElement, chip: RefChipData) => void;
  unmount: (element: HTMLElement) => void;
};

class ChipWidget extends WidgetType {
  readonly chip: RefChipData;
  readonly host: SlotHost;
  constructor(chip: RefChipData, host: SlotHost) {
    super();
    this.chip = chip;
    this.host = host;
  }
  eq(other: ChipWidget) {
    return (
      other.chip.kind === this.chip.kind &&
      other.chip.label === this.chip.label &&
      other.chip.detail === this.chip.detail &&
      other.chip.thumb === this.chip.thumb
    );
  }
  toDOM() {
    const element = document.createElement("span");
    element.className = "cm-ref-chip";
    this.host.mount(element, this.chip);
    return element;
  }
  destroy(element: HTMLElement) {
    this.host.unmount(element);
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
  className,
  minHeight,
  maxHeight,
  attributes,
  onChange,
  onCursor,
  onKeyDown,
  onPaste,
  onCopy,
  ref,
}: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const handlers = useRef({ onChange, onCursor, onKeyDown, onPaste, onCopy });
  handlers.current = { onChange, onCursor, onKeyDown, onPaste, onCopy };
  const [slots, setSlots] = useState<ReadonlyMap<HTMLElement, RefChipData>>(new Map());
  const slotIds = useRef(new WeakMap<HTMLElement, number>());
  const nextSlot = useRef(0);
  const slotHost = useRef<SlotHost>({
    mount: (element, chip) => {
      slotIds.current.set(element, nextSlot.current++);
      setSlots((current) => new Map(current).set(element, chip));
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
              handlers.current.onChange(update.state.doc.toString(), head, typed);
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
        .filter((chip) => chip.to <= value.length && chip.from < chip.to)
        .map((chip) =>
          Decoration.replace({ widget: new ChipWidget(chip.chip, slotHost.current) }).range(
            chip.from,
            chip.to,
          ),
        ),
      true,
    );
    if (current === value) {
      view.dispatch({ effects: setChips.of(decorations) });
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
      effects: setChips.of(decorations),
    });
  }, [value, chips]);

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
    }),
    [],
  );

  return (
    <div ref={hostRef} className={cn("w-full", className)}>
      {[...slots].map(([element, chip]) =>
        createPortal(<RefChip chip={chip} />, element, String(slotIds.current.get(element))),
      )}
    </div>
  );
}
