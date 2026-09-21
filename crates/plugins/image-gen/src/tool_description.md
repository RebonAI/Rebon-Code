Generates an image from a description, or edits existing images following specific instructions, with OpenAI's gpt-image-2. Use it when:

- The user asks for an image from a scene description, such as a diagram, portrait, comic, meme, illustration, texture, sprite, mockup, or any other bitmap visual.
- The user wants to modify an image — add or remove elements, change colors, improve quality or resolution, change the style (e.g. cartoon, oil painting), or cut the subject out onto a transparent background.

Guidelines:
- A call takes one to a few minutes. Issue one call per requested image or variant; calls may run in parallel.
- Omit `referenced_image_paths` when generating a brand-new image.
- For edits and reference-guided generation, pass the absolute path of every target and reference image in `referenced_image_paths` (at most 5). Images generated earlier in this session can be passed by the saved path their result reported.
- If you have not seen a local image yet, look at it with `Read` before editing it.
- An image that exists only inline in the conversation, with no file behind it, cannot be passed; ask the user to save or attach it as a file.
- For a transparent background, ask for it in the prompt; the result keeps the generated alpha channel.
- Generate directly, without asking for reconfirmation, unless a required image has to be provided again.
- Always use this tool for image generation and editing unless the user explicitly asks otherwise. Do not write scripts to call an image API or to edit images yourself.
- The result reports where the image was saved. Copy it to wherever the project needs it; do not render it again as a Markdown image.
- Load the `imagegen` skill for prompting guidance before a non-trivial request.
