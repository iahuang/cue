; Text outside `<?php ?>` is HTML. Each stretch is parsed on its own, so a
; tag split by PHP comes out as two broken ones.
((text) @injection.content
  (#set! injection.language "html"))
