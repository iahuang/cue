; Headings. An ATX heading is captured with its section, so that sections
; nest; setext headings don't start sections.

(section
  (atx_heading
    heading_content: (inline) @name)) @definition.heading

(setext_heading
  heading_content: (paragraph
    (inline) @name)) @definition.heading
