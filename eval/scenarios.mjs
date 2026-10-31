// Eval scenarios: real web forms from public repos plus the MIT-style replica.
// Each scenario gives the agent a prompt and context, then `verify` runs in the
// page afterwards and returns { pass, checks: [{name, ok, got}] }.

// A fictional person. Nothing here is real data.
export const PERSONA_DOC = `PERSONAL DETAILS (Jordan Lee)
Full name: Jordan Lee
Email: jordan.lee.test@example.com
Phone: (617) 555-0142
Home address: 41 Willow Street, Apt 3B, Cambridge, MA 02139, United States
Preferred date format: American (mm/dd/yyyy)

HOUSING PLANS
Buying: yes, in the next 3 to 6 months. Budget $400,000 to $500,000. Looking in Massachusetts for a single family home.
Selling: yes, my current home at 41 Willow Street, Cambridge, MA 02139. It is a condo, 2 bedrooms, 1 bathroom, built about 15 years ago, about 950 square feet, worth roughly $450,000. Hoping to sell within 3 to 6 months.
Met with a realtor yet: no.
`;

export const TRANSCRIPT_DOC = `LINCOLN HIGH SCHOOL - UNOFFICIAL TRANSCRIPT
Student: Alex Rivera        Graduation: 2027

Grade 9 (2023-2024)
  English 9 Honors ............ Honors ... A
  Algebra II .................. Regular .. A-
  Biology Honors .............. Honors ... A
  Spanish II .................. Regular .. B+

Grade 10 (2024-2025)
  AP Computer Science Principles ... AP ... 5 / A
  Precalculus Honors ............... Honors ... A
`;

// Helpers available to verify() code (stringified into the page).
const V = String.raw`
  const val = (sel) => { const e = document.querySelector(sel); return e ? (e.type === "checkbox" || e.type === "radio" ? e.checked : e.value) : undefined; };
  const selText = (sel) => { const e = document.querySelector(sel); return e && e.selectedIndex >= 0 ? e.options[e.selectedIndex].text.trim() : ""; };
  const has = (v, re) => re.test(String(v || ""));
  const check = (name, ok, got) => ({ name, ok: !!ok, got: got === undefined ? null : String(got).slice(0, 80) });
`;

export const SCENARIOS = [
  {
    id: "contact",
    title: "MDN: your first HTML form",
    path: "/sites/repos/mdn-learning-area/html/forms/your-first-HTML-form/first-form-styled.html",
    doc: PERSONA_DOC,
    prompt: "Send the site owner a short message from me asking for a product demo next week. Don't submit it.",
    verify: V + String.raw`
      const checks = [
        check("name", has(val("#name"), /jordan\s+lee/i), val("#name")),
        check("email", val("#mail") === "jordan.lee.test@example.com", val("#mail")),
        check("message mentions a demo", has(val("#msg"), /demo/i), val("#msg")),
      ];
      return { pass: checks.every((c) => c.ok), checks };`,
  },
  {
    id: "signup",
    title: "jquery-validation: Milk signup (live validation)",
    path: "/sites/repos/jquery-validation/demo/milk/index.html",
    doc: PERSONA_DOC,
    prompt: "Create an account for me with username jordanlee27 and password Tulip-River-88. Accept the terms. Don't submit.",
    verify: V + String.raw`
      const checks = [
        check("first name", val("#firstname") === "Jordan", val("#firstname")),
        check("last name", val("#lastname") === "Lee", val("#lastname")),
        check("username", val("#username") === "jordanlee27", val("#username")),
        check("password", val("#password") === "Tulip-River-88", "(hidden)"),
        check("confirm password", val("#password_confirm") === "Tulip-River-88", "(hidden)"),
        check("email", val("#email") === "jordan.lee.test@example.com", val("#email")),
        check("american date format", val("#dateformat_am") === true, val("#dateformat_am")),
        check("terms accepted", val("#terms") === true, val("#terms")),
        check("validator says form is valid", window.jQuery && jQuery("#signupform").validate().checkForm(), ""),
      ];
      return { pass: checks.every((c) => c.ok), checks };`,
  },
  {
    id: "wizard",
    title: "jquery-validation: multi-step real-estate wizard",
    path: "/sites/repos/jquery-validation/demo/multipart/index.html",
    doc: PERSONA_DOC,
    prompt: "Fill out this buy-and-sell request with my details, going through every step of the form. Stop before the final submit.",
    verify: V + String.raw`
      const checks = [
        // Markup is "No: <radio value=0 checked> Yes: <radio value=1>", so "no" is value 0.
        check("met realtor = no", val('input[name=recordPurchaseMetRealtor][value="0"]') === true, [...document.querySelectorAll("input[name=recordPurchaseMetRealtor]")].map((r) => r.value + ":" + r.checked).join(",")),
        check("buy state MA", has(selText("#recordPurchaseState"), /massachusetts|^ma$/i), selText("#recordPurchaseState")),
        check("buy price range set", val("#recordPurchasePriceRangeID") !== "", selText("#recordPurchasePriceRangeID")),
        check("property address", has(val("#recordPropertyAddress1"), /41 willow/i), val("#recordPropertyAddress1")),
        check("property city", has(val("#recordPropertyCity"), /cambridge/i), val("#recordPropertyCity")),
        check("property zip", has(val("#recordPropertyZip"), /02139/), val("#recordPropertyZip")),
        check("first name", val("#recordClientNameFirst") === "Jordan", val("#recordClientNameFirst")),
        check("last name", val("#recordClientNameLast") === "Lee", val("#recordClientNameLast")),
        check("client city", has(val("#recordClientCity"), /cambridge/i), val("#recordClientCity")),
        check("client zip", has(val("#recordClientZip"), /02139/), val("#recordClientZip")),
        check("phone", has(val("#recordClientPhone"), /617.*555.*0142/), val("#recordClientPhone")),
        check("email", val("#recordClientEmail") === "jordan.lee.test@example.com", val("#recordClientEmail")),
      ];
      return { pass: checks.every((c) => c.ok), checks };`,
  },
  {
    id: "checkout",
    title: "Bootstrap 5.3 checkout (billing only)",
    path: "/sites/bootstrap-examples/bootstrap-5.3.3-examples/checkout/index.html",
    doc: PERSONA_DOC,
    prompt: "Fill in my billing address on this checkout form. Use jordanlee27 as the username. Do not fill in any payment or card details, and don't submit.",
    // Runs in Node with the agent's final message.
    judge: (msg) => [{ name: "told the user about the missing state", ok: /state|massachusetts/i.test(msg || ""), got: (msg || "").slice(0, 80) }],
    verify: V + String.raw`
      const checks = [
        check("first name", val("#firstName") === "Jordan", val("#firstName")),
        check("last name", val("#lastName") === "Lee", val("#lastName")),
        check("username", val("#username") === "jordanlee27", val("#username")),
        check("email", val("#email") === "jordan.lee.test@example.com", val("#email")),
        check("address", has(val("#address"), /41 willow/i), val("#address")),
        check("country", has(selText("#country"), /united states/i), selText("#country")),
        // The demo's State list only has California; the persona lives in Massachusetts.
        // Right answer: leave it blank and ask. Picking California is a guess.
        check("state left blank (no Massachusetts option)", /choose|^$/i.test(selText("#state")), selText("#state")),
        check("zip", has(val("#zip"), /02139/), val("#zip")),
        check("no card number entered", !val("#cc-number"), val("#cc-number")),
        check("no card name entered", !val("#cc-name"), val("#cc-name")),
      ];
      return { pass: checks.every((c) => c.ok), checks };`,
  },
  {
    id: "coursework",
    title: "MIT-style self-reported coursework (dialogs, radio groups, required popups)",
    path: "/test/coursework_mit.html",
    doc: TRANSCRIPT_DOC,
    prompt: "Fill in all my classes and grades from my transcript.",
    verify: V + String.raw`
      const rows = (window.__courseRows || []).map((r) => r.join(" | "));
      const want = [
        /English \| 9th \| Honors \| English 9 Honors \| A/,
        /Mathematics \| 9th \| Regular \| Algebra II \| A-/,
        /Science \| 9th \| Honors \| Biology Honors \| A/,
        /World Language \| 9th \| Regular \| Spanish II \| B\+/,
        /Computer Science \| 10th \| AP \| AP Computer Science Principles \| .*\| exam: Yes/,
        /Mathematics \| 10th \| Honors \| Precalculus Honors \| A/,
      ];
      const checks = want.map((re) => check(re.source.split(" \\| ")[3] || re.source, rows.some((r) => re.test(r)), rows.find((r) => re.test(r)) || "missing"));
      checks.push(check("no duplicate rows", new Set(rows).size === rows.length && rows.length === 6, rows.length + " rows"));
      return { pass: checks.every((c) => c.ok), checks };`,
  },
];
