var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __commonJS = (cb, mod) => function __require() {
  try {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  } catch (e) {
    throw mod = 0, e;
  }
};
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
  // If the importer is in node compatibility mode or this is not an ESM
  // file that has been converted to a CommonJS file using a Babel-
  // compatible transform (i.e. "__esModule" has not been set), then set
  // "default" to the CommonJS "module.exports" for node compatibility.
  isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
  mod
));

// node_modules/.pnpm/@joplin+turndown-plugin-gfm@1.0.67/node_modules/@joplin/turndown-plugin-gfm/lib/turndown-plugin-gfm.cjs.js
var require_turndown_plugin_gfm_cjs = __commonJS({
  "node_modules/.pnpm/@joplin+turndown-plugin-gfm@1.0.67/node_modules/@joplin/turndown-plugin-gfm/lib/turndown-plugin-gfm.cjs.js"(exports) {
    "use strict";
    Object.defineProperty(exports, "__esModule", { value: true });
    var highlightRegExp = /highlight-(?:text|source)-([a-z0-9]+)/;
    function highlightedCodeBlock(turndownService) {
      turndownService.addRule("highlightedCodeBlock", {
        filter: function(node) {
          var firstChild = node.firstChild;
          return node.nodeName === "DIV" && highlightRegExp.test(node.className) && firstChild && firstChild.nodeName === "PRE";
        },
        replacement: function(content, node, options) {
          var className = node.className || "";
          var language = (className.match(highlightRegExp) || [null, ""])[1];
          return "\n\n" + options.fence + language + "\n" + node.firstChild.textContent + "\n" + options.fence + "\n\n";
        }
      });
    }
    function strikethrough(turndownService) {
      turndownService.addRule("strikethrough", {
        filter: ["del", "s", "strike"],
        replacement: function(content) {
          return "~~" + content + "~~";
        }
      });
    }
    var indexOf = Array.prototype.indexOf;
    var every = Array.prototype.every;
    var rules2 = {};
    var alignMap = { left: ":---", right: "---:", center: ":---:" };
    var isCodeBlock_ = null;
    var options_ = null;
    var tableShouldBeSkippedCache_ = /* @__PURE__ */ new WeakMap();
    function getAlignment(node) {
      return node ? (node.getAttribute("align") || node.style.textAlign || "").toLowerCase() : "";
    }
    function getBorder(alignment) {
      return alignment ? alignMap[alignment] : "---";
    }
    function getColumnAlignment(table, columnIndex) {
      var votes = {
        left: 0,
        right: 0,
        center: 0,
        "": 0
      };
      var align = "";
      for (var i = 0; i < table.rows.length; ++i) {
        var row = table.rows[i];
        if (columnIndex < row.childNodes.length) {
          var cellAlignment = getAlignment(row.childNodes[columnIndex]);
          ++votes[cellAlignment];
          if (votes[cellAlignment] > votes[align]) {
            align = cellAlignment;
          }
        }
      }
      return align;
    }
    rules2.tableCell = {
      filter: ["th", "td"],
      replacement: function(content, node) {
        if (tableShouldBeSkipped(nodeParentTable(node))) return content;
        return cell(content, node);
      }
    };
    rules2.tableRow = {
      filter: "tr",
      replacement: function(content, node) {
        const parentTable = nodeParentTable(node);
        if (tableShouldBeSkipped(parentTable)) return content;
        var borderCells = "";
        if (isHeadingRow(node)) {
          const colCount = tableColCount(parentTable);
          for (var i = 0; i < colCount; i++) {
            const childNode = i < node.childNodes.length ? node.childNodes[i] : null;
            var border = getBorder(getColumnAlignment(parentTable, i));
            borderCells += cell(border, childNode, i);
          }
        }
        return "\n" + content + (borderCells ? "\n" + borderCells : "");
      }
    };
    rules2.table = {
      filter: function(node, options) {
        return node.nodeName === "TABLE";
      },
      replacement: function(content, node) {
        if (tableShouldBeHtml(node, options_)) {
          let html = node.outerHTML;
          let divParent = nodeParentDiv(node);
          if (divParent === null || !divParent.classList.contains("joplin-table-wrapper")) {
            return `

<div class="joplin-table-wrapper">${html}</div>

`;
          } else {
            return html;
          }
        } else {
          if (tableShouldBeSkipped(node)) return content;
          content = content.replace(/\n+/g, "\n");
          var secondLine = content.trim().split("\n");
          if (secondLine.length >= 2) secondLine = secondLine[1];
          var secondLineIsDivider = /\| :?---/.test(secondLine);
          var columnCount = tableColCount(node);
          var emptyHeader = "";
          if (columnCount && !secondLineIsDivider) {
            emptyHeader = "|" + "     |".repeat(columnCount) + "\n|";
            for (var columnIndex = 0; columnIndex < columnCount; ++columnIndex) {
              emptyHeader += " " + getBorder(getColumnAlignment(node, columnIndex)) + " |";
            }
          }
          const captionNode = node.querySelector ? node.querySelector("caption") : node.caption;
          const captionContent = captionNode ? captionNode.textContent || "" : "";
          const caption = captionContent ? `${captionContent}

` : "";
          const tableContent = `${emptyHeader}${content}`.trimStart();
          return `

${caption}${tableContent}

`;
        }
      }
    };
    rules2.tableCaption = {
      filter: ["caption"],
      replacement: () => ""
    };
    rules2.tableColgroup = {
      filter: ["colgroup", "col"],
      replacement: () => ""
    };
    rules2.tableSection = {
      filter: ["thead", "tbody", "tfoot"],
      replacement: function(content) {
        return content;
      }
    };
    function isHeadingRow(tr) {
      var parentNode = tr.parentNode;
      return parentNode.nodeName === "THEAD" || parentNode.firstChild === tr && (parentNode.nodeName === "TABLE" || isFirstTbody(parentNode)) && every.call(tr.childNodes, function(n) {
        return n.nodeName === "TH";
      });
    }
    function isFirstTbody(element) {
      var previousSibling = element.previousSibling;
      return element.nodeName === "TBODY" && (!previousSibling || previousSibling.nodeName === "THEAD" && /^\s*$/i.test(previousSibling.textContent));
    }
    function cell(content, node = null, index = null) {
      if (index === null) index = indexOf.call(node.parentNode.childNodes, node);
      var prefix = " ";
      if (index === 0) prefix = "| ";
      let filteredContent = content.trim().replace(/\n\r/g, "<br>").replace(/\n/g, "<br>");
      filteredContent = filteredContent.replace(/\|+/g, "\\|");
      while (filteredContent.length < 3) filteredContent += " ";
      if (node) filteredContent = handleColSpan(filteredContent, node, " ");
      return prefix + filteredContent + " |";
    }
    function nodeContainsTable(node) {
      if (!node.childNodes) return false;
      for (let i = 0; i < node.childNodes.length; i++) {
        const child = node.childNodes[i];
        if (child.nodeName === "TABLE") return true;
        if (nodeContainsTable(child)) return true;
      }
      return false;
    }
    var nodeContains = (node, types) => {
      if (!node.childNodes) return false;
      for (let i = 0; i < node.childNodes.length; i++) {
        const child = node.childNodes[i];
        if (types === "code" && isCodeBlock_ && isCodeBlock_(child)) return true;
        if (types.includes(child.nodeName)) return true;
        if (nodeContains(child, types)) return true;
      }
      return false;
    };
    var customStyleProperties = [
      "background-color",
      "background",
      "border-color",
      "border",
      "border-top",
      "border-right",
      "border-bottom",
      "border-left",
      "border-style",
      "border-width",
      "padding",
      "padding-top",
      "padding-right",
      "padding-bottom",
      "padding-left",
      "float",
      "margin-left",
      "margin-right"
    ];
    var customAttributeNames = [
      "bgcolor",
      "bordercolor",
      "background"
    ];
    var nodeHasCustomStyle = (node) => {
      if (!node || !node.getAttribute) return false;
      const styleAttr = node.getAttribute("style");
      if (!styleAttr) return false;
      const properties = styleAttr.split(";").map((s) => s.split(":")[0].trim().toLowerCase()).filter((s) => s.length > 0);
      for (let i = 0; i < properties.length; i++) {
        if (customStyleProperties.includes(properties[i])) return true;
      }
      return false;
    };
    var hasNonDefaultSpacingAttribute = (node, name2) => {
      if (!node || !node.getAttribute) return false;
      const value = node.getAttribute(name2);
      if (value === null) return false;
      const normalisedValue = `${value}`.trim().toLowerCase();
      if (!normalisedValue) return false;
      if (normalisedValue === "0" || normalisedValue === "0px") return false;
      return true;
    };
    var nodeHasCustomAttributes = (node) => {
      if (!node || !node.getAttribute) return false;
      for (let i = 0; i < customAttributeNames.length; i++) {
        const value = node.getAttribute(customAttributeNames[i]);
        if (value !== null && `${value}`.trim() !== "") return true;
      }
      if (node.nodeName === "TABLE") {
        if (hasNonDefaultSpacingAttribute(node, "cellpadding")) return true;
        if (hasNonDefaultSpacingAttribute(node, "cellspacing")) return true;
      }
      return false;
    };
    var nodeHasCustomFormatting = (node) => {
      return nodeHasCustomStyle(node) || nodeHasCustomAttributes(node);
    };
    var tableHasCustomStyles = (tableNode) => {
      if (nodeHasCustomFormatting(tableNode)) return true;
      const rows = tableNode.rows;
      if (!rows) return false;
      for (let i = 0; i < rows.length; i++) {
        const row = rows[i];
        if (nodeHasCustomFormatting(row)) return true;
        for (let j = 0; j < row.childNodes.length; j++) {
          const cell2 = row.childNodes[j];
          if ((cell2.nodeName === "TD" || cell2.nodeName === "TH") && nodeHasCustomFormatting(cell2)) {
            return true;
          }
        }
      }
      return false;
    };
    var tableShouldBeHtml = (tableNode, options) => {
      const possibleTags = [
        "UL",
        "OL",
        "H1",
        "H2",
        "H3",
        "H4",
        "H5",
        "H6",
        "HR",
        "BLOCKQUOTE"
      ];
      if (options.preserveNestedTables) possibleTags.push("TABLE");
      return nodeContains(tableNode, "code") || nodeContains(tableNode, possibleTags) || options.preserveTableStyles && tableHasCustomStyles(tableNode);
    };
    function tableShouldBeSkipped(tableNode) {
      const cached = tableShouldBeSkippedCache_.get(tableNode);
      if (cached !== void 0) return cached;
      const result = tableShouldBeSkipped_(tableNode);
      tableShouldBeSkippedCache_.set(tableNode, result);
      return result;
    }
    function tableShouldBeSkipped_(tableNode) {
      if (!tableNode) return true;
      if (!tableNode.rows) return true;
      if (tableNode.rows.length === 1 && tableNode.rows[0].childNodes.length <= 1) return true;
      if (nodeContainsTable(tableNode)) return true;
      return false;
    }
    function nodeParentDiv(node) {
      let parent = node.parentNode;
      while (parent.nodeName !== "DIV") {
        parent = parent.parentNode;
        if (!parent) return null;
      }
      return parent;
    }
    function nodeParentTable(node) {
      let parent = node.parentNode;
      while (parent.nodeName !== "TABLE") {
        parent = parent.parentNode;
        if (!parent) return null;
      }
      return parent;
    }
    function handleColSpan(content, node, emptyChar) {
      const colspan = node.getAttribute("colspan") || 1;
      for (let i = 1; i < colspan; i++) {
        content += " | " + emptyChar.repeat(3);
      }
      return content;
    }
    function tableColCount(node) {
      let maxColCount = 0;
      for (let i = 0; i < node.rows.length; i++) {
        const row = node.rows[i];
        const colCount = row.childNodes.length;
        if (colCount > maxColCount) maxColCount = colCount;
      }
      return maxColCount;
    }
    function tables(turndownService) {
      isCodeBlock_ = turndownService.isCodeBlock;
      options_ = turndownService.options;
      turndownService.keep(function(node) {
        if (node.nodeName === "TABLE" && tableShouldBeHtml(node, turndownService.options)) return true;
        return false;
      });
      for (var key in rules2) turndownService.addRule(key, rules2[key]);
    }
    function taskListItems(turndownService) {
      turndownService.addRule("taskListItems", {
        filter: function(node) {
          const parent = node.parentNode;
          const grandparent = parent.parentNode;
          const grandparentIsListItem = !!grandparent && grandparent.nodeName === "LI";
          return (node.type === "checkbox" || node.getAttribute("role") === "checkbox") && (parent.nodeName === "LI" || parent.nodeName === "LABEL" && grandparentIsListItem || parent.nodeName === "SPAN" && grandparentIsListItem);
        },
        replacement: function(content, node) {
          const checked = node.nodeName === "INPUT" ? node.checked : node.getAttribute("aria-checked") === "true";
          return (checked ? "[x]" : "[ ]") + " ";
        }
      });
    }
    function gfm2(turndownService) {
      turndownService.use([
        highlightedCodeBlock,
        strikethrough,
        tables,
        taskListItems
      ]);
    }
    exports.gfm = gfm2;
    exports.highlightedCodeBlock = highlightedCodeBlock;
    exports.strikethrough = strikethrough;
    exports.tables = tables;
    exports.taskListItems = taskListItems;
  }
});

// packages/web/tool-web/src/index.ts
import z from "@deepseek-ai/schemastery";

// packages/web/tool-web/src/search.ts
import { defineTool } from "@deepseek-ai/dsh-tools";
var WEB_SEARCH_MAX_RESULTS = 8;
function parseSearchArgs(args) {
  if (args.query.trim().length === 0) throw new Error("query must be a non-empty string");
  return { query: args.query };
}
function sourceLabel(url, title) {
  if (title !== void 0 && title.length > 0) return title;
  try {
    return new URL(url).hostname;
  } catch {
    return url;
  }
}
function formatSearchOutput(result) {
  const parts = [];
  if (result.content !== void 0 && result.content.length > 0) parts.push(result.content);
  if (result.sources.length > 0) {
    const lines = result.sources.map((source) => {
      const label = sourceLabel(source.url, source.title);
      const meta = [];
      if (source.snippet !== void 0 && source.snippet.length > 0) meta.push(source.snippet);
      if (source.publishedAt !== void 0 && source.publishedAt.length > 0) meta.push(`(${source.publishedAt})`);
      const suffix = meta.length > 0 ? ` \u2014 ${meta.join(" ")}` : "";
      return `- [${label}](${source.url})${suffix}`;
    });
    parts.push(`Sources:
${lines.join("\n")}`);
  } else if (result.content === void 0 || result.content.length === 0) {
    parts.push("No results found.");
  }
  if (result.truncated) parts.push(`(Showing the first ${result.sources.length} sources. Refine the query for more.)`);
  parts.push("Cite the relevant URLs above as markdown links in your answer.");
  return parts.join("\n\n");
}
function presentSearchCall(args) {
  return { card: "generic", title: args.query, kind: "search", rawInput: args.query };
}
function projectSource(source) {
  return {
    url: source.url,
    ...source.title !== void 0 ? { title: source.title } : {},
    ...source.snippet !== void 0 ? { snippet: source.snippet } : {},
    ...source.publishedAt !== void 0 ? { publishedAt: source.publishedAt } : {}
  };
}
function searchMetaFromValue(value) {
  return {
    sources: value.sources.map(projectSource),
    truncated: value.truncated,
    ...value.content !== void 0 ? { answer: value.content } : {}
  };
}
function isWebSource(value) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const { url, title, snippet, publishedAt } = value;
  return typeof url === "string" && (title === void 0 || typeof title === "string") && (snippet === void 0 || typeof snippet === "string") && (publishedAt === void 0 || typeof publishedAt === "string");
}
function searchMetaFromResult(meta) {
  if (typeof meta !== "object" || meta === null || Array.isArray(meta)) return void 0;
  const { sources, truncated, answer } = meta;
  if (!Array.isArray(sources) || !sources.every(isWebSource)) return void 0;
  if (typeof truncated !== "boolean") return void 0;
  if (answer !== void 0 && typeof answer !== "string") return void 0;
  return {
    sources,
    truncated,
    ...answer !== void 0 ? { answer } : {}
  };
}
function presentSearchResult(args, result) {
  if (result.isError) return void 0;
  const meta = searchMetaFromResult(result.meta);
  if (meta === void 0) return void 0;
  return {
    card: "web",
    kind: "search",
    title: args.query,
    sources: meta.sources,
    truncated: meta.truncated,
    ...meta.answer !== void 0 ? { answer: meta.answer } : {}
  };
}
function applyWebSearchTool(ctx, maxResults, timeoutMs, fetchEnabled) {
  ctx.systemPrompt.section({
    name: "tool:web_search",
    order: 110,
    text: fetchEnabled ? "Use the web_search tool to discover current information on the web. It returns an optional answer plus a list of source URLs. Follow up with web_fetch when you need the full content of a specific result, and cite the relevant URLs as markdown links." : "Use the web_search tool to discover current information on the web. It returns an optional answer plus a list of source URLs. Use the returned source snippets when available, and cite the relevant URLs as markdown links."
  });
  ctx.tools.register(defineTool({
    name: "web_search",
    description: "Search the web for current information. Returns an optional summary answer and a list of source URLs.",
    parameters: {
      query: { type: "string", required: true, description: "The search query." }
    },
    output: {
      schema: {
        type: "object",
        additionalProperties: false,
        properties: {
          content: { type: "string" },
          sources: {
            type: "array",
            required: true,
            items: {
              type: "object",
              additionalProperties: false,
              properties: {
                url: { type: "string", required: true },
                title: { type: "string" },
                snippet: { type: "string" },
                publishedAt: { type: "string" }
              }
            }
          },
          truncated: { type: "boolean", required: true }
        }
      },
      render: (_args, value) => [{ type: "text", text: formatSearchOutput(value) }],
      presentationMeta: (_args, value) => searchMetaFromValue(value)
    },
    timeoutMs,
    // Provider reads do not mutate parent-agent state.
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      const input = parseSearchArgs(args);
      const result = await ctx.web.search(
        { query: input.query, maxResults },
        exec.signal
      );
      return {
        ...result.content !== void 0 ? { content: result.content } : {},
        sources: result.sources.map(projectSource),
        truncated: result.truncated
      };
    },
    presentCall: presentSearchCall,
    presentResult: (args, result) => presentSearchResult(args, result)
  }));
}

// node_modules/.pnpm/turndown@7.2.4/node_modules/turndown/lib/turndown.browser.es.js
function extend(destination) {
  for (var i = 1; i < arguments.length; i++) {
    var source = arguments[i];
    for (var key in source) {
      if (Object.prototype.hasOwnProperty.call(source, key)) destination[key] = source[key];
    }
  }
  return destination;
}
function repeat(character, count) {
  return Array(count + 1).join(character);
}
function trimLeadingNewlines(string) {
  return string.replace(/^\n*/, "");
}
function trimTrailingNewlines(string) {
  var indexEnd = string.length;
  while (indexEnd > 0 && string[indexEnd - 1] === "\n") indexEnd--;
  return string.substring(0, indexEnd);
}
function trimNewlines(string) {
  return trimTrailingNewlines(trimLeadingNewlines(string));
}
var blockElements = ["ADDRESS", "ARTICLE", "ASIDE", "AUDIO", "BLOCKQUOTE", "BODY", "CANVAS", "CENTER", "DD", "DIR", "DIV", "DL", "DT", "FIELDSET", "FIGCAPTION", "FIGURE", "FOOTER", "FORM", "FRAMESET", "H1", "H2", "H3", "H4", "H5", "H6", "HEADER", "HGROUP", "HR", "HTML", "ISINDEX", "LI", "MAIN", "MENU", "NAV", "NOFRAMES", "NOSCRIPT", "OL", "OUTPUT", "P", "PRE", "SECTION", "TABLE", "TBODY", "TD", "TFOOT", "TH", "THEAD", "TR", "UL"];
function isBlock(node) {
  return is(node, blockElements);
}
var voidElements = ["AREA", "BASE", "BR", "COL", "COMMAND", "EMBED", "HR", "IMG", "INPUT", "KEYGEN", "LINK", "META", "PARAM", "SOURCE", "TRACK", "WBR"];
function isVoid(node) {
  return is(node, voidElements);
}
function hasVoid(node) {
  return has(node, voidElements);
}
var meaningfulWhenBlankElements = ["A", "TABLE", "THEAD", "TBODY", "TFOOT", "TH", "TD", "IFRAME", "SCRIPT", "AUDIO", "VIDEO"];
function isMeaningfulWhenBlank(node) {
  return is(node, meaningfulWhenBlankElements);
}
function hasMeaningfulWhenBlank(node) {
  return has(node, meaningfulWhenBlankElements);
}
function is(node, tagNames) {
  return tagNames.indexOf(node.nodeName) >= 0;
}
function has(node, tagNames) {
  return node.getElementsByTagName && tagNames.some(function(tagName) {
    return node.getElementsByTagName(tagName).length;
  });
}
var markdownEscapes = [[/\\/g, "\\\\"], [/\*/g, "\\*"], [/^-/g, "\\-"], [/^\+ /g, "\\+ "], [/^(=+)/g, "\\$1"], [/^(#{1,6}) /g, "\\$1 "], [/`/g, "\\`"], [/^~~~/g, "\\~~~"], [/\[/g, "\\["], [/\]/g, "\\]"], [/^>/g, "\\>"], [/_/g, "\\_"], [/^(\d+)\. /g, "$1\\. "]];
function escapeMarkdown(string) {
  return markdownEscapes.reduce(function(accumulator, escape) {
    return accumulator.replace(escape[0], escape[1]);
  }, string);
}
var rules = {};
rules.paragraph = {
  filter: "p",
  replacement: function(content) {
    return "\n\n" + content + "\n\n";
  }
};
rules.lineBreak = {
  filter: "br",
  replacement: function(content, node, options) {
    return options.br + "\n";
  }
};
rules.heading = {
  filter: ["h1", "h2", "h3", "h4", "h5", "h6"],
  replacement: function(content, node, options) {
    var hLevel = Number(node.nodeName.charAt(1));
    if (options.headingStyle === "setext" && hLevel < 3) {
      var underline = repeat(hLevel === 1 ? "=" : "-", content.length);
      return "\n\n" + content + "\n" + underline + "\n\n";
    } else {
      return "\n\n" + repeat("#", hLevel) + " " + content + "\n\n";
    }
  }
};
rules.blockquote = {
  filter: "blockquote",
  replacement: function(content) {
    content = trimNewlines(content).replace(/^/gm, "> ");
    return "\n\n" + content + "\n\n";
  }
};
rules.list = {
  filter: ["ul", "ol"],
  replacement: function(content, node) {
    var parent = node.parentNode;
    if (parent.nodeName === "LI" && parent.lastElementChild === node) {
      return "\n" + content;
    } else {
      return "\n\n" + content + "\n\n";
    }
  }
};
rules.listItem = {
  filter: "li",
  replacement: function(content, node, options) {
    var prefix = options.bulletListMarker + "   ";
    var parent = node.parentNode;
    if (parent.nodeName === "OL") {
      var start = parent.getAttribute("start");
      var index = Array.prototype.indexOf.call(parent.children, node);
      prefix = (start ? Number(start) + index : index + 1) + ".  ";
    }
    var isParagraph = /\n$/.test(content);
    content = trimNewlines(content) + (isParagraph ? "\n" : "");
    content = content.replace(/\n/gm, "\n" + " ".repeat(prefix.length));
    return prefix + content + (node.nextSibling ? "\n" : "");
  }
};
rules.indentedCodeBlock = {
  filter: function(node, options) {
    return options.codeBlockStyle === "indented" && node.nodeName === "PRE" && node.firstChild && node.firstChild.nodeName === "CODE";
  },
  replacement: function(content, node, options) {
    return "\n\n    " + node.firstChild.textContent.replace(/\n/g, "\n    ") + "\n\n";
  }
};
rules.fencedCodeBlock = {
  filter: function(node, options) {
    return options.codeBlockStyle === "fenced" && node.nodeName === "PRE" && node.firstChild && node.firstChild.nodeName === "CODE";
  },
  replacement: function(content, node, options) {
    var className = node.firstChild.getAttribute("class") || "";
    var language = (className.match(/language-(\S+)/) || [null, ""])[1];
    var code = node.firstChild.textContent;
    var fenceChar = options.fence.charAt(0);
    var fenceSize = 3;
    var fenceInCodeRegex = new RegExp("^" + fenceChar + "{3,}", "gm");
    var match;
    while (match = fenceInCodeRegex.exec(code)) {
      if (match[0].length >= fenceSize) {
        fenceSize = match[0].length + 1;
      }
    }
    var fence = repeat(fenceChar, fenceSize);
    return "\n\n" + fence + language + "\n" + code.replace(/\n$/, "") + "\n" + fence + "\n\n";
  }
};
rules.horizontalRule = {
  filter: "hr",
  replacement: function(content, node, options) {
    return "\n\n" + options.hr + "\n\n";
  }
};
rules.inlineLink = {
  filter: function(node, options) {
    return options.linkStyle === "inlined" && node.nodeName === "A" && node.getAttribute("href");
  },
  replacement: function(content, node) {
    var href = escapeLinkDestination(node.getAttribute("href"));
    var title = escapeLinkTitle(cleanAttribute(node.getAttribute("title")));
    var titlePart = title ? ' "' + title + '"' : "";
    return "[" + content + "](" + href + titlePart + ")";
  }
};
rules.referenceLink = {
  filter: function(node, options) {
    return options.linkStyle === "referenced" && node.nodeName === "A" && node.getAttribute("href");
  },
  replacement: function(content, node, options) {
    var href = escapeLinkDestination(node.getAttribute("href"));
    var title = cleanAttribute(node.getAttribute("title"));
    if (title) title = ' "' + escapeLinkTitle(title) + '"';
    var replacement;
    var reference;
    switch (options.linkReferenceStyle) {
      case "collapsed":
        replacement = "[" + content + "][]";
        reference = "[" + content + "]: " + href + title;
        break;
      case "shortcut":
        replacement = "[" + content + "]";
        reference = "[" + content + "]: " + href + title;
        break;
      default:
        var id = this.references.length + 1;
        replacement = "[" + content + "][" + id + "]";
        reference = "[" + id + "]: " + href + title;
    }
    this.references.push(reference);
    return replacement;
  },
  references: [],
  append: function(options) {
    var references = "";
    if (this.references.length) {
      references = "\n\n" + this.references.join("\n") + "\n\n";
      this.references = [];
    }
    return references;
  }
};
rules.emphasis = {
  filter: ["em", "i"],
  replacement: function(content, node, options) {
    if (!content.trim()) return "";
    return options.emDelimiter + content + options.emDelimiter;
  }
};
rules.strong = {
  filter: ["strong", "b"],
  replacement: function(content, node, options) {
    if (!content.trim()) return "";
    return options.strongDelimiter + content + options.strongDelimiter;
  }
};
rules.code = {
  filter: function(node) {
    var hasSiblings = node.previousSibling || node.nextSibling;
    var isCodeBlock = node.parentNode.nodeName === "PRE" && !hasSiblings;
    return node.nodeName === "CODE" && !isCodeBlock;
  },
  replacement: function(content) {
    if (!content) return "";
    content = content.replace(/\r?\n|\r/g, " ");
    var extraSpace = /^`|^ .*?[^ ].* $|`$/.test(content) ? " " : "";
    var delimiter = "`";
    var matches = content.match(/`+/gm) || [];
    while (matches.indexOf(delimiter) !== -1) delimiter = delimiter + "`";
    return delimiter + extraSpace + content + extraSpace + delimiter;
  }
};
rules.image = {
  filter: "img",
  replacement: function(content, node) {
    var alt = escapeMarkdown(cleanAttribute(node.getAttribute("alt")));
    var src = escapeLinkDestination(node.getAttribute("src") || "");
    var title = cleanAttribute(node.getAttribute("title"));
    var titlePart = title ? ' "' + escapeLinkTitle(title) + '"' : "";
    return src ? "![" + alt + "](" + src + titlePart + ")" : "";
  }
};
function cleanAttribute(attribute) {
  return attribute ? attribute.replace(/(\n+\s*)+/g, "\n") : "";
}
function escapeLinkDestination(destination) {
  var escaped = destination.replace(/([<>()])/g, "\\$1");
  return escaped.indexOf(" ") >= 0 ? "<" + escaped + ">" : escaped;
}
function escapeLinkTitle(title) {
  return title.replace(/"/g, '\\"');
}
function Rules(options) {
  this.options = options;
  this._keep = [];
  this._remove = [];
  this.blankRule = {
    replacement: options.blankReplacement
  };
  this.keepReplacement = options.keepReplacement;
  this.defaultRule = {
    replacement: options.defaultReplacement
  };
  this.array = [];
  for (var key in options.rules) this.array.push(options.rules[key]);
}
Rules.prototype = {
  add: function(key, rule) {
    this.array.unshift(rule);
  },
  keep: function(filter) {
    this._keep.unshift({
      filter,
      replacement: this.keepReplacement
    });
  },
  remove: function(filter) {
    this._remove.unshift({
      filter,
      replacement: function() {
        return "";
      }
    });
  },
  forNode: function(node) {
    if (node.isBlank) return this.blankRule;
    var rule;
    if (rule = findRule(this.array, node, this.options)) return rule;
    if (rule = findRule(this._keep, node, this.options)) return rule;
    if (rule = findRule(this._remove, node, this.options)) return rule;
    return this.defaultRule;
  },
  forEach: function(fn) {
    for (var i = 0; i < this.array.length; i++) fn(this.array[i], i);
  }
};
function findRule(rules2, node, options) {
  for (var i = 0; i < rules2.length; i++) {
    var rule = rules2[i];
    if (filterValue(rule, node, options)) return rule;
  }
  return void 0;
}
function filterValue(rule, node, options) {
  var filter = rule.filter;
  if (typeof filter === "string") {
    if (filter === node.nodeName.toLowerCase()) return true;
  } else if (Array.isArray(filter)) {
    if (filter.indexOf(node.nodeName.toLowerCase()) > -1) return true;
  } else if (typeof filter === "function") {
    if (filter.call(rule, node, options)) return true;
  } else {
    throw new TypeError("`filter` needs to be a string, array, or function");
  }
}
function collapseWhitespace(options) {
  var element = options.element;
  var isBlock2 = options.isBlock;
  var isVoid2 = options.isVoid;
  var isPre = options.isPre || function(node2) {
    return node2.nodeName === "PRE";
  };
  if (!element.firstChild || isPre(element)) return;
  var prevText = null;
  var keepLeadingWs = false;
  var prev = null;
  var node = next(prev, element, isPre);
  while (node !== element) {
    if (node.nodeType === 3 || node.nodeType === 4) {
      var text = node.data.replace(/[ \r\n\t]+/g, " ");
      if ((!prevText || / $/.test(prevText.data)) && !keepLeadingWs && text[0] === " ") {
        text = text.substr(1);
      }
      if (!text) {
        node = remove(node);
        continue;
      }
      node.data = text;
      prevText = node;
    } else if (node.nodeType === 1) {
      if (isBlock2(node) || node.nodeName === "BR") {
        if (prevText) {
          prevText.data = prevText.data.replace(/ $/, "");
        }
        prevText = null;
        keepLeadingWs = false;
      } else if (isVoid2(node) || isPre(node)) {
        prevText = null;
        keepLeadingWs = true;
      } else if (prevText) {
        keepLeadingWs = false;
      }
    } else {
      node = remove(node);
      continue;
    }
    var nextNode = next(prev, node, isPre);
    prev = node;
    node = nextNode;
  }
  if (prevText) {
    prevText.data = prevText.data.replace(/ $/, "");
    if (!prevText.data) {
      remove(prevText);
    }
  }
}
function remove(node) {
  var next2 = node.nextSibling || node.parentNode;
  node.parentNode.removeChild(node);
  return next2;
}
function next(prev, current, isPre) {
  if (prev && prev.parentNode === current || isPre(current)) {
    return current.nextSibling || current.parentNode;
  }
  return current.firstChild || current.nextSibling || current.parentNode;
}
var root = typeof window !== "undefined" ? window : {};
function canParseHTMLNatively() {
  var Parser = root.DOMParser;
  var canParse = false;
  try {
    if (new Parser().parseFromString("", "text/html")) {
      canParse = true;
    }
  } catch (e) {
  }
  return canParse;
}
function createHTMLParser() {
  var Parser = function() {
  };
  {
    if (shouldUseActiveX()) {
      Parser.prototype.parseFromString = function(string) {
        var doc = new window.ActiveXObject("htmlfile");
        doc.designMode = "on";
        doc.open();
        doc.write(string);
        doc.close();
        return doc;
      };
    } else {
      Parser.prototype.parseFromString = function(string) {
        var doc = document.implementation.createHTMLDocument("");
        doc.open();
        doc.write(string);
        doc.close();
        return doc;
      };
    }
  }
  return Parser;
}
function shouldUseActiveX() {
  var useActiveX = false;
  try {
    document.implementation.createHTMLDocument("").open();
  } catch (e) {
    if (root.ActiveXObject) useActiveX = true;
  }
  return useActiveX;
}
var HTMLParser = canParseHTMLNatively() ? root.DOMParser : createHTMLParser();
function RootNode(input, options) {
  var root2;
  if (typeof input === "string") {
    var doc = htmlParser().parseFromString(
      // DOM parsers arrange elements in the <head> and <body>.
      // Wrapping in a custom element ensures elements are reliably arranged in
      // a single element.
      '<x-turndown id="turndown-root">' + input + "</x-turndown>",
      "text/html"
    );
    root2 = doc.getElementById("turndown-root");
  } else {
    root2 = input.cloneNode(true);
  }
  collapseWhitespace({
    element: root2,
    isBlock,
    isVoid,
    isPre: options.preformattedCode ? isPreOrCode : null
  });
  return root2;
}
var _htmlParser;
function htmlParser() {
  _htmlParser = _htmlParser || new HTMLParser();
  return _htmlParser;
}
function isPreOrCode(node) {
  return node.nodeName === "PRE" || node.nodeName === "CODE";
}
function Node(node, options) {
  node.isBlock = isBlock(node);
  node.isCode = node.nodeName === "CODE" || node.parentNode.isCode;
  node.isBlank = isBlank(node);
  node.flankingWhitespace = flankingWhitespace(node, options);
  return node;
}
function isBlank(node) {
  return !isVoid(node) && !isMeaningfulWhenBlank(node) && /^\s*$/i.test(node.textContent) && !hasVoid(node) && !hasMeaningfulWhenBlank(node);
}
function flankingWhitespace(node, options) {
  if (node.isBlock || options.preformattedCode && node.isCode) {
    return {
      leading: "",
      trailing: ""
    };
  }
  var edges = edgeWhitespace(node.textContent);
  if (edges.leadingAscii && isFlankedByWhitespace("left", node, options)) {
    edges.leading = edges.leadingNonAscii;
  }
  if (edges.trailingAscii && isFlankedByWhitespace("right", node, options)) {
    edges.trailing = edges.trailingNonAscii;
  }
  return {
    leading: edges.leading,
    trailing: edges.trailing
  };
}
function edgeWhitespace(string) {
  var m = string.match(/^(([ \t\r\n]*)(\s*))(?:(?=\S)[\s\S]*\S)?((\s*?)([ \t\r\n]*))$/);
  return {
    leading: m[1],
    // whole string for whitespace-only strings
    leadingAscii: m[2],
    leadingNonAscii: m[3],
    trailing: m[4],
    // empty for whitespace-only strings
    trailingNonAscii: m[5],
    trailingAscii: m[6]
  };
}
function isFlankedByWhitespace(side, node, options) {
  var sibling;
  var regExp;
  var isFlanked;
  if (side === "left") {
    sibling = node.previousSibling;
    regExp = / $/;
  } else {
    sibling = node.nextSibling;
    regExp = /^ /;
  }
  if (sibling) {
    if (sibling.nodeType === 3) {
      isFlanked = regExp.test(sibling.nodeValue);
    } else if (options.preformattedCode && sibling.nodeName === "CODE") {
      isFlanked = false;
    } else if (sibling.nodeType === 1 && !isBlock(sibling)) {
      isFlanked = regExp.test(sibling.textContent);
    }
  }
  return isFlanked;
}
var reduce = Array.prototype.reduce;
function TurndownService(options) {
  if (!(this instanceof TurndownService)) return new TurndownService(options);
  var defaults = {
    rules,
    headingStyle: "setext",
    hr: "* * *",
    bulletListMarker: "*",
    codeBlockStyle: "indented",
    fence: "```",
    emDelimiter: "_",
    strongDelimiter: "**",
    linkStyle: "inlined",
    linkReferenceStyle: "full",
    br: "  ",
    preformattedCode: false,
    blankReplacement: function(content, node) {
      return node.isBlock ? "\n\n" : "";
    },
    keepReplacement: function(content, node) {
      return node.isBlock ? "\n\n" + node.outerHTML + "\n\n" : node.outerHTML;
    },
    defaultReplacement: function(content, node) {
      return node.isBlock ? "\n\n" + content + "\n\n" : content;
    }
  };
  this.options = extend({}, defaults, options);
  this.rules = new Rules(this.options);
}
TurndownService.prototype = {
  /**
   * The entry point for converting a string or DOM node to Markdown
   * @public
   * @param {String|HTMLElement} input The string or DOM node to convert
   * @returns A Markdown representation of the input
   * @type String
   */
  turndown: function(input) {
    if (!canConvert(input)) {
      throw new TypeError(input + " is not a string, or an element/document/fragment node.");
    }
    if (input === "") return "";
    var output = process.call(this, new RootNode(input, this.options));
    return postProcess.call(this, output);
  },
  /**
   * Add one or more plugins
   * @public
   * @param {Function|Array} plugin The plugin or array of plugins to add
   * @returns The Turndown instance for chaining
   * @type Object
   */
  use: function(plugin) {
    if (Array.isArray(plugin)) {
      for (var i = 0; i < plugin.length; i++) this.use(plugin[i]);
    } else if (typeof plugin === "function") {
      plugin(this);
    } else {
      throw new TypeError("plugin must be a Function or an Array of Functions");
    }
    return this;
  },
  /**
   * Adds a rule
   * @public
   * @param {String} key The unique key of the rule
   * @param {Object} rule The rule
   * @returns The Turndown instance for chaining
   * @type Object
   */
  addRule: function(key, rule) {
    this.rules.add(key, rule);
    return this;
  },
  /**
   * Keep a node (as HTML) that matches the filter
   * @public
   * @param {String|Array|Function} filter The unique key of the rule
   * @returns The Turndown instance for chaining
   * @type Object
   */
  keep: function(filter) {
    this.rules.keep(filter);
    return this;
  },
  /**
   * Remove a node that matches the filter
   * @public
   * @param {String|Array|Function} filter The unique key of the rule
   * @returns The Turndown instance for chaining
   * @type Object
   */
  remove: function(filter) {
    this.rules.remove(filter);
    return this;
  },
  /**
   * Escapes Markdown syntax
   * @public
   * @param {String} string The string to escape
   * @returns A string with Markdown syntax escaped
   * @type String
   */
  escape: function(string) {
    return escapeMarkdown(string);
  }
};
function process(parentNode) {
  var self = this;
  return reduce.call(parentNode.childNodes, function(output, node) {
    node = new Node(node, self.options);
    var replacement = "";
    if (node.nodeType === 3) {
      replacement = node.isCode ? node.nodeValue : self.escape(node.nodeValue);
    } else if (node.nodeType === 1) {
      replacement = replacementForNode.call(self, node);
    }
    return join(output, replacement);
  }, "");
}
function postProcess(output) {
  var self = this;
  this.rules.forEach(function(rule) {
    if (typeof rule.append === "function") {
      output = join(output, rule.append(self.options));
    }
  });
  return output.replace(/^[\t\r\n]+/, "").replace(/[\t\r\n\s]+$/, "");
}
function replacementForNode(node) {
  var rule = this.rules.forNode(node);
  var content = process.call(this, node);
  var whitespace = node.flankingWhitespace;
  if (whitespace.leading || whitespace.trailing) content = content.trim();
  return whitespace.leading + rule.replacement(content, node, this.options) + whitespace.trailing;
}
function join(output, replacement) {
  var s1 = trimTrailingNewlines(output);
  var s2 = trimLeadingNewlines(replacement);
  var nls = Math.max(output.length - s1.length, replacement.length - s2.length);
  var separator = "\n\n".substring(0, nls);
  return s1 + separator + s2;
}
function canConvert(input) {
  return input != null && (typeof input === "string" || input.nodeType && (input.nodeType === 1 || input.nodeType === 9 || input.nodeType === 11));
}

// packages/web/tool-web/src/fetch.ts
var import_turndown_plugin_gfm = __toESM(require_turndown_plugin_gfm_cjs(), 1);
import { defineTool as defineTool2 } from "@deepseek-ai/dsh-tools";
import { assertNever } from "@deepseek-ai/dsh-llm";
var turndown = new TurndownService({
  headingStyle: "atx",
  codeBlockStyle: "fenced",
  bulletListMarker: "-"
});
turndown.use(import_turndown_plugin_gfm.gfm);
turndown.remove(["script", "style", "noscript"]);
function renderTableCell(content, index) {
  const prefix = index === 0 ? "| " : " ";
  const escaped = content.trim().replace(/\n\r/g, "<br>").replace(/\n/g, "<br>").replace(/\|+/g, "\\|").padEnd(3, " ");
  return `${prefix}${escaped} |`;
}
function isTableHeadingRow(row) {
  const cells = Array.from(row.cells);
  const section = row.parentElement;
  const table = section.parentElement;
  return (section.nodeName === "THEAD" || table.rows[0] === row) && cells.every((cell) => cell.nodeName === "TH");
}
function tableBorder(cell) {
  const alignment = (cell.getAttribute("align") || cell.style.textAlign || "").toLowerCase();
  if (alignment === "left") return ":---";
  if (alignment === "right") return "---:";
  if (alignment === "center") return ":---:";
  return "---";
}
turndown.addRule("tableCellWithoutSpanExpansion", {
  filter: ["th", "td"],
  replacement(content, node) {
    const cell = node;
    const row = cell.parentNode;
    return renderTableCell(content, Array.prototype.indexOf.call(row.childNodes, cell));
  }
});
turndown.addRule("tableRowWithoutSpanExpansion", {
  filter: "tr",
  replacement(content, node) {
    const row = node;
    const border = isTableHeadingRow(row) ? Array.from(row.cells, (cell, index) => renderTableCell(tableBorder(cell), index)).join("") : "";
    return `
${content}${border.length > 0 ? `
${border}` : ""}`;
  }
});
function parseFetchArgs(args) {
  if (args.url.trim().length === 0) throw new Error("url must be a non-empty string");
  return { url: args.url };
}
var MAX_CONVERSION_DEPTH = 512;
var VOID_ELEMENTS = /* @__PURE__ */ new Set([
  "area",
  "base",
  "br",
  "col",
  "embed",
  "hr",
  "img",
  "input",
  "link",
  "meta",
  "param",
  "source",
  "track",
  "wbr"
]);
var RAW_TEXT_ELEMENTS = /* @__PURE__ */ new Set(["script", "style", "noscript"]);
function isTagBoundary(char) {
  return char === void 0 || char === ">" || char === "/" || /\s/.test(char);
}
function findRawTextEnd(lowerHtml, name2, from) {
  const prefix = `</${name2}`;
  let candidate = lowerHtml.indexOf(prefix, from);
  while (candidate !== -1 && !isTagBoundary(lowerHtml[candidate + prefix.length])) {
    candidate = lowerHtml.indexOf(prefix, candidate + prefix.length);
  }
  return candidate;
}
function exceedsConversionDepth(html) {
  const lowerHtml = html.toLowerCase();
  const openElements = [];
  let offset = 0;
  let inComment = false;
  while (offset < html.length) {
    const start = html.indexOf("<", offset);
    if (inComment) {
      const end = html.indexOf("-->", offset);
      if (end !== -1 && (start === -1 || end < start)) {
        inComment = false;
        offset = end + 3;
        continue;
      }
    }
    if (start === -1) break;
    if (!inComment && html.startsWith("<!--", start)) {
      inComment = true;
      offset = start + 4;
      continue;
    }
    let cursor = start + 1;
    const closing = html[cursor] === "/";
    if (closing) cursor += 1;
    const nameStart = cursor;
    while (/[a-zA-Z0-9-]/.test(html[cursor] ?? "")) cursor += 1;
    if (cursor === nameStart || !/[a-zA-Z]/.test(html.charAt(nameStart))) {
      offset = start + 1;
      continue;
    }
    const name2 = lowerHtml.slice(nameStart, cursor);
    let quote;
    while (cursor < html.length) {
      const char = html[cursor];
      cursor += 1;
      if (quote !== void 0) {
        if (char === quote) quote = void 0;
      } else if (char === '"' || char === "'") {
        quote = char;
      } else if (char === ">") {
        break;
      }
    }
    if (html[cursor - 1] !== ">") break;
    if (closing) {
      if (!inComment && openElements.at(-1) === name2) openElements.pop();
    } else {
      let last = cursor - 2;
      while (/\s/.test(html.charAt(last))) last -= 1;
      if (!VOID_ELEMENTS.has(name2) && html[last] !== "/") {
        openElements.push(name2);
        if (openElements.length > MAX_CONVERSION_DEPTH) return true;
        if (!inComment && RAW_TEXT_ELEMENTS.has(name2)) {
          const end = findRawTextEnd(lowerHtml, name2, cursor);
          if (end === -1) break;
          offset = end;
          continue;
        }
      }
    }
    offset = cursor;
  }
  return false;
}
function renderBody(body, maxInputChars) {
  const content = body.content.slice(0, maxInputChars);
  const sourceTruncated = content.length !== body.content.length;
  switch (body.kind) {
    case "html":
      if (exceedsConversionDepth(content)) return { text: content, sourceTruncated };
      try {
        return { text: turndown.turndown(content), sourceTruncated };
      } catch {
        return { text: content, sourceTruncated };
      }
    case "text":
      return { text: content, sourceTruncated };
    /* v8 ignore next 2 -- WebFetchBody is a closed union; this arm is unreachable and only makes adding a kind a compile error. */
    default:
      return assertNever(body, "unhandled web fetch body kind");
  }
}
var TRUNCATION_FOOTER = "\n\n(Content truncated. Fetch a more specific URL or section for the full text.)";
function renderFetchOutput(result, maxOutputChars) {
  const byCap = renderCache.get(result) ?? /* @__PURE__ */ new Map();
  const cached = byCap.get(maxOutputChars);
  if (cached !== void 0) return cached;
  const computed = computeFetchOutput(result, maxOutputChars);
  byCap.set(maxOutputChars, computed);
  renderCache.set(result, byCap);
  return computed;
}
var renderCache = /* @__PURE__ */ new WeakMap();
function computeFetchOutput(result, maxOutputChars) {
  const header = `Fetched ${result.url} (HTTP ${result.statusCode})

`;
  const rendered = renderBody(result.body, maxOutputChars);
  const prefix = `${header}${rendered.text}`;
  const truncated = result.truncated || rendered.sourceTruncated || prefix.length > maxOutputChars;
  const full = `${prefix}${truncated ? TRUNCATION_FOOTER : ""}`;
  if (full.length <= maxOutputChars) return { text: full, truncated };
  if (maxOutputChars < TRUNCATION_FOOTER.length) return { text: full.slice(0, maxOutputChars), truncated };
  return { text: `${prefix.slice(0, maxOutputChars - TRUNCATION_FOOTER.length)}${TRUNCATION_FOOTER}`, truncated };
}
function formatFetchOutput(result, maxOutputChars) {
  return renderFetchOutput(result, maxOutputChars).text;
}
function presentFetchCall(args) {
  return { card: "generic", title: args.url, kind: "fetch", rawInput: args.url };
}
function fetchMetaFromValue(value, maxOutputChars) {
  return { url: value.url, statusCode: value.statusCode, truncated: renderFetchOutput(value, maxOutputChars).truncated };
}
function fetchMetaFromResult(meta) {
  if (typeof meta !== "object" || meta === null || Array.isArray(meta)) return void 0;
  const { url, statusCode, truncated } = meta;
  if (typeof url !== "string" || typeof statusCode !== "number" || typeof truncated !== "boolean") return void 0;
  return { url, statusCode, truncated };
}
function presentFetchResult(args, result) {
  if (result.isError) return void 0;
  const meta = fetchMetaFromResult(result.meta);
  if (meta === void 0) return void 0;
  return {
    card: "web",
    kind: "fetch",
    title: args.url,
    url: meta.url,
    statusCode: meta.statusCode,
    truncated: meta.truncated
  };
}
function applyWebFetchTool(ctx, timeoutMs, maxOutputChars) {
  ctx.systemPrompt.section({
    name: "tool:web_fetch",
    order: 111,
    text: "Use the web_fetch tool to retrieve the content of a specific HTTP(S) URL (for example a result from web_search). It returns the page content decoded to text. Cite the URL as a markdown link when you use its content."
  });
  ctx.tools.register(defineTool2({
    name: "web_fetch",
    description: "Fetch the content of a specific HTTP(S) URL and return it decoded to text.",
    parameters: {
      url: { type: "string", required: true, description: "The HTTP(S) URL to fetch." }
    },
    output: {
      schema: {
        type: "object",
        additionalProperties: false,
        properties: {
          url: { type: "string", required: true },
          statusCode: { type: "integer", required: true },
          body: {
            required: true,
            oneOf: [
              {
                type: "object",
                additionalProperties: false,
                properties: {
                  kind: { type: "string", required: true, const: "html" },
                  content: { type: "string", required: true }
                }
              },
              {
                type: "object",
                additionalProperties: false,
                properties: {
                  kind: { type: "string", required: true, const: "text" },
                  content: { type: "string", required: true }
                }
              }
            ]
          },
          truncated: { type: "boolean", required: true }
        }
      },
      render: (_args, value) => [{ type: "text", text: formatFetchOutput(value, maxOutputChars) }],
      presentationMeta: (_args, value) => fetchMetaFromValue(value, maxOutputChars)
    },
    timeoutMs,
    // Provider reads do not mutate parent-agent state.
    isConcurrencySafe: () => true,
    async execute(args, exec) {
      const input = parseFetchArgs(args);
      const result = await ctx.web.fetch(
        { url: input.url },
        exec.signal
      );
      return {
        url: result.url,
        statusCode: result.statusCode,
        body: { kind: result.body.kind, content: result.body.content },
        truncated: result.truncated
      };
    },
    presentCall: presentFetchCall,
    presentResult: (args, result) => presentFetchResult(args, result)
  }));
}

// packages/web/tool-web/src/index.ts
var name = "tool-web";
var inject = ["tools", "web", "systemPrompt"];
var DEFAULT_WEB_TOOL_TIMEOUT_MS = 3e4;
var DEFAULT_FETCH_MAX_OUTPUT_CHARS = 2e5;
var Config = z.object({
  search: z.boolean().default(true),
  fetch: z.boolean().default(true),
  searchMaxResults: z.number().default(WEB_SEARCH_MAX_RESULTS),
  fetchTimeoutMs: z.number().default(DEFAULT_WEB_TOOL_TIMEOUT_MS),
  searchTimeoutMs: z.number().default(DEFAULT_WEB_TOOL_TIMEOUT_MS),
  fetchMaxOutputChars: z.number().default(DEFAULT_FETCH_MAX_OUTPUT_CHARS)
});
function assertPositiveInteger(name2, value) {
  if (!Number.isInteger(value) || value < 1) {
    throw new Error(`tool-web: ${name2} must be a positive integer`);
  }
}
function apply(ctx, config) {
  const resolved = config;
  assertPositiveInteger("searchMaxResults", resolved.searchMaxResults);
  assertPositiveInteger("fetchTimeoutMs", resolved.fetchTimeoutMs);
  assertPositiveInteger("searchTimeoutMs", resolved.searchTimeoutMs);
  assertPositiveInteger("fetchMaxOutputChars", resolved.fetchMaxOutputChars);
  if (resolved.search) {
    applyWebSearchTool(ctx, resolved.searchMaxResults, resolved.searchTimeoutMs, resolved.fetch);
  }
  if (resolved.fetch) applyWebFetchTool(ctx, resolved.fetchTimeoutMs, resolved.fetchMaxOutputChars);
}
export {
  Config,
  DEFAULT_FETCH_MAX_OUTPUT_CHARS,
  DEFAULT_WEB_TOOL_TIMEOUT_MS,
  WEB_SEARCH_MAX_RESULTS,
  apply,
  applyWebFetchTool,
  applyWebSearchTool,
  fetchMetaFromResult,
  fetchMetaFromValue,
  formatFetchOutput,
  formatSearchOutput,
  inject,
  name,
  parseFetchArgs,
  parseSearchArgs,
  presentFetchCall,
  presentFetchResult,
  presentSearchCall,
  presentSearchResult,
  searchMetaFromResult,
  searchMetaFromValue
};
