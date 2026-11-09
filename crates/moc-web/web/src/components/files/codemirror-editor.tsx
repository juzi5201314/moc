// CodeMirror 6 编辑器：Geist Mono + 令牌派生主题，语言按文件名懒加载。
// 深浅色各一套配色，与对话区 shiki 代码块（github-light/dark）协调；
// 悬停、聚焦都由默认样式承担，只有当前行给极淡底色。

import { history } from "@codemirror/commands";
import { HighlightStyle, LanguageDescription, syntaxHighlighting } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { Compartment, EditorState, type Extension } from "@codemirror/state";
import { drawSelection, EditorView, highlightActiveLine, lineNumbers } from "@codemirror/view";
import { tags } from "@lezer/highlight";
import { useEffect, useMemo, useRef } from "react";
import { useIsDark } from "@/lib/use-dark";

/** 编辑器各可变配置分 Compartment 管，改动时只 reconfigure 不重建（保住撤销栈与滚动）。 */
const langConf = new Compartment();
const themeConf = new Compartment();
const wrapConf = new Compartment();
const readonlyConf = new Compartment();

export function CodeMirrorEditor({
  fileName,
  content,
  editable,
  wrap,
  line,
  lineNonce,
  onChange,
  onDirty,
}: {
  fileName: string;
  content: string;
  editable: boolean;
  wrap: boolean;
  line?: number;
  /** 跳转行的触发计数：同一文件再带行号打开时自增。 */
  lineNonce: number;
  onChange: (text: string) => void;
  onDirty?: () => void;
}) {
  const dark = useIsDark();
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  // 编辑器内部 onChange 与外部 setState 互相触发的回环保护。
  const syncing = useRef(false);
  const dirtyRef = useRef(onDirty);
  dirtyRef.current = onDirty;
  const changeRef = useRef(onChange);
  changeRef.current = onChange;

  const theme = useMemo(() => buildTheme(dark), [dark]);

  // 只在初始化/fileName 变化时重建；content/主题/换行/只读各走独立的同步与 reconfigure effect。
  // biome-ignore lint/correctness/useExhaustiveDependencies: content/editable/wrap/theme 故意不进依赖——它们走下面的独立 effect
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: content,
        extensions: [
          lineNumbers(),
          highlightActiveLine(),
          history(),
          drawSelection(),
          langConf.of([]),
          themeConf.of(theme),
          wrapConf.of(wrap ? EditorView.lineWrapping : []),
          readonlyConf.of([EditorState.readOnly.of(!editable), EditorView.editable.of(editable)]),
          EditorView.updateListener.of((update) => {
            if (update.docChanged && !syncing.current) {
              dirtyRef.current?.();
              changeRef.current(update.state.doc.toString());
            }
          }),
        ],
      }),
    });
    viewRef.current = view;
    // 语言懒加载：LanguageDescription.matchFilename 只给描述，load() 之后才有 support。
    const desc = LanguageDescription.matchFilename(languages, fileName);
    if (desc) {
      void desc.load().then((support) => {
        if (viewRef.current === view) {
          view.dispatch({ effects: langConf.reconfigure(support) });
        }
      });
    }
    return () => {
      view.destroy();
      viewRef.current = null;
    };
  }, [fileName]);

  // 外部内容变化（重载磁盘版本等）→ 无痕迹地替换文档。
  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    if (view.state.doc.toString() !== content) {
      syncing.current = true;
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: content } });
      syncing.current = false;
    }
  }, [content]);

  // 主题 / 换行 / 只读：只 reconfigure 对应 Compartment。
  useEffect(() => {
    viewRef.current?.dispatch({ effects: themeConf.reconfigure(theme) });
  }, [theme]);
  useEffect(() => {
    viewRef.current?.dispatch({
      effects: wrapConf.reconfigure(wrap ? EditorView.lineWrapping : []),
    });
  }, [wrap]);
  useEffect(() => {
    viewRef.current?.dispatch({
      effects: readonlyConf.reconfigure([
        EditorState.readOnly.of(!editable),
        EditorView.editable.of(editable),
      ]),
    });
  }, [editable]);

  // 行号跳转：lineNonce 每次自增都滚一次（同文件重复带行号打开也要生效）。
  useEffect(() => {
    const view = viewRef.current;
    if (!view || !line || lineNonce === 0) return;
    const clamped = Math.max(1, Math.min(line, view.state.doc.lines));
    const pos = view.state.doc.line(clamped).from;
    view.dispatch({
      selection: { anchor: pos },
      effects: EditorView.scrollIntoView(pos, { y: "center" }),
    });
    view.focus();
  }, [line, lineNonce]);

  return <div ref={hostRef} className="moc-cm min-h-0 flex-1 overflow-auto" />;
}

/**
 * 与对话区 shiki 的 github-light/github-dark 同一套色谱：
 * 关键字/字符串/数字/注释等 token 色直接对齐，避免编辑器与聊天代码观感分裂。
 */
function buildTheme(dark: boolean): Extension {
  const c = dark
    ? {
        fg: "#e1e4e8",
        keyword: "#f97583",
        string: "#9ecbff",
        number: "#79b8ff",
        comment: "#959da5",
        fn: "#b392f0",
        type: "#79b8ff",
        property: "#79b8ff",
        operator: "#f97583",
        variable: "#e1e4e8",
        punctuation: "#e1e4e8",
        bool: "#79b8ff",
      }
    : {
        fg: "#24292e",
        keyword: "#d73a49",
        string: "#032f62",
        number: "#005cc5",
        comment: "#6a737d",
        fn: "#6f42c1",
        type: "#005cc5",
        property: "#005cc5",
        operator: "#d73a49",
        // github-light 的橙色只用于参数；普通变量保持前景色。
        variable: "#24292e",
        punctuation: "#24292e",
        bool: "#005cc5",
      };
  const highlight = HighlightStyle.define([
    {
      tag: [tags.keyword, tags.modifier, tags.operatorKeyword, tags.controlKeyword],
      color: c.keyword,
    },
    { tag: [tags.string, tags.special(tags.string), tags.regexp], color: c.string },
    { tag: [tags.number, tags.integer, tags.float, tags.unit], color: c.number },
    { tag: [tags.bool, tags.null, tags.atom], color: c.bool },
    { tag: tags.comment, color: c.comment, fontStyle: "italic" },
    { tag: [tags.function(tags.variableName), tags.function(tags.propertyName)], color: c.fn },
    {
      tag: [tags.typeName, tags.className, tags.tagName, tags.standard(tags.typeName)],
      color: c.type,
    },
    { tag: [tags.propertyName, tags.attributeName], color: c.property },
    { tag: tags.operator, color: c.operator },
    { tag: tags.variableName, color: c.variable },
    { tag: tags.punctuation, color: c.punctuation },
  ]);
  const chrome = EditorView.theme(
    {
      "&": {
        height: "100%",
        fontSize: "13px",
        backgroundColor: "transparent",
      },
      ".cm-content": {
        fontFamily: "var(--font-mono)",
        caretColor: "var(--brand)",
        padding: "12px 0",
      },
      ".cm-line": { padding: "0 16px" },
      ".cm-cursor": { borderLeftColor: "var(--brand)" },
      "&.cm-focused": { outline: "none" },
      ".cm-gutters": {
        backgroundColor: "transparent",
        border: "none",
        color: "var(--text-tertiary)",
        fontFamily: "var(--font-mono)",
        fontSize: "11px",
        paddingLeft: "8px",
      },
      ".cm-activeLineGutter": { backgroundColor: "transparent", color: "var(--foreground)" },
      ".cm-activeLine": {
        backgroundColor: "color-mix(in oklab, var(--foreground) 4%, transparent)",
      },
      "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": {
        backgroundColor: "var(--selection) !important",
      },
      ".cm-selectionMatch": {
        backgroundColor: "color-mix(in oklab, var(--brand) 14%, transparent)",
      },
      ".cm-matchingBracket": {
        backgroundColor: "color-mix(in oklab, var(--brand) 18%, transparent)",
        outline: "none",
      },
    },
    { dark },
  );
  return [chrome, syntaxHighlighting(highlight)];
}
