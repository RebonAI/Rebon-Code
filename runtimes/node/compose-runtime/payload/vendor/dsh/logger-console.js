// dsh-vendor/logger-console/src/shared.ts
import { Logger } from "@deepseek-ai/cordis";
import { Time } from "@deepseek-ai/cosmokit";
import z from "@deepseek-ai/schemastery";
var ConsoleExporter = class {
  constructor(ctx, config = {}) {
    this.ctx = ctx;
    Object.assign(this, this.getDefaults(), config);
    this.timestamp = Date.now();
    ctx.logger.exporter(this);
  }
  ctx;
  static name = "logger-console";
  static Config = z.object({
    colors: z.union([z.const(false), z.number()]),
    maxLength: z.number(),
    levels: z.dict(z.number()),
    showDiff: z.boolean().default(false),
    showTime: z.string().default("yyyy-MM-dd hh:mm:ss "),
    label: z.object({
      width: z.number(),
      margin: z.number(),
      align: z.union(["left", "right"])
    })
  });
  colors;
  maxLength;
  levels;
  showDiff;
  showTime;
  label;
  timestamp;
  formatters = {};
  getDefaults() {
    return {
      colors: false,
      showTime: "yyyy-MM-dd hh:mm:ss ",
      showDiff: false
    };
  }
  export(message) {
    console.log(this.render(message));
  }
  render(message) {
    const prefix = `[${message.type[0].toUpperCase()}]`;
    const space = " ".repeat(this.label?.margin ?? 1);
    let indent = 3 + space.length, output = "";
    if (this.showTime) {
      indent += this.showTime.length;
      output += Logger.color(this, 8, Time.template(this.showTime));
    }
    const code = Logger.code(message.name, this.colors);
    const label = Logger.color(this, code, message.name, ";1");
    const padLength = (this.label?.width ?? 0) + label.length - message.name.length;
    if (this.label?.align === "right") {
      output += label.padStart(padLength) + space + prefix + space;
      indent += (this.label.width ?? 0) + space.length;
    } else {
      output += prefix + space + label.padEnd(padLength) + space;
    }
    output += Logger.format(this, message).replace(/\n/g, "\n" + " ".repeat(indent));
    if (this.showDiff && this.timestamp) {
      const diff = message.ts - this.timestamp;
      output += Logger.color(this, code, " +" + Time.format(diff));
    }
    this.timestamp = message.ts;
    return output;
  }
};

// dsh-vendor/logger-console/src/browser.ts
var ConsoleExporter2 = class extends ConsoleExporter {
  export(message) {
    const prefix = `[${message.type[0].toUpperCase()}] ${message.name}`;
    const method = message.type === "error" ? "error" : message.type === "warn" ? "warn" : "log";
    console[method](prefix, ...message.args);
  }
};
var browser_default = ConsoleExporter2;
export {
  ConsoleExporter2 as ConsoleExporter,
  browser_default as default
};
