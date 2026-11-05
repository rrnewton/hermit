// Hand-written declarations for the browser globals voice.js reads that TypeScript's DOM library
// does not describe. The wire contract's types are generated separately, in contract.d.ts.

interface Window {
  /** Safari's prefixed Web Audio constructor, used when `AudioContext` is absent. */
  webkitAudioContext?: typeof AudioContext;
}

interface HTMLLIElement {
  /**
   * The message objects a rendered row stands for: one on an ordinary row, several on a combined
   * one. Set by voice.js when it draws the row.
   */
  messages?: VibeTalk.Message[];
  /**
   * `#211 link-filter`. The text the Links filter reads a row's links from, on a row that does not
   * stand for channel messages: a voice turn, or a message still being sent.
   */
  linkText?: string;
  /** `#228 reply-visible`. A channel row's Reply button, whose label says how many replies it has. */
  replyButton?: HTMLButtonElement;
  /** `#228 reply-visible`. A reply's arrow, on its row, for the pass that asks which arrows meet the row above. */
  replyArrowButton?: HTMLButtonElement;
  /** `#228 reply-visible`. The circle on the row's lower-left corner, while it has one: the way down to a reply. */
  replyMarker?: HTMLButtonElement | null;
  /** `#193 compose-draft-loss`. A channel row's Draft, shown while a reply to it is saved unsent. */
  draftMarker?: HTMLButtonElement;
}

interface HTMLElement {
  /**
   * `#240 reading-bookmark`. The bookmark's line on the row it is drawn above, made once and kept on
   * the row, as a collapsed run's line is. Set by voice.js (`bookmarkLineOf`).
   */
  bookmarkLine?: HTMLElement;
}
