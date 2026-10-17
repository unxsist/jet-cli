# JET (Thuisbezorgd) API research — 2026-10-17

Unofficial, reverse-engineered from the www.thuisbezorgd.nl web app (Next.js "OneWeb",
`consumer-web` v0.1.17858) and its hosted login SPA. Expect this to drift.

## Hosts

| Purpose | Base URL |
|---|---|
| Auth (OIDC, Duende IdentityServer) | **`https://auth.thuisbezorgd.nl`** for NL (brand host; issuer `https://auth.jet-external.com`). Also reachable as `auth.eu-central-1.production.jet-external.com`, but Turnstile only works on the brand host. Other brands: `auth.lieferando.de`, `auth.takeaway.com`, `auth.just-eat.co.uk`, … (`authenticationServiceURL` in the consumer-web bundle). |
| JE REST APIs (EU) | `https://rest.api.eu-central-1.production.jet-external.com` (alias `https://i18n.api.just-eat.io`) |
| Legacy Takeaway BFF | `https://cw-api.takeaway.com` (BFF version `34`; needs BFF token, see below) |
| Menu CDN | `https://globalmenucdn.eu-central-1.production.jet-external.com` |

Tenant for NL is `nl`; most paths are `/{area}/{tenant}/...` and requests carry `accept-tenant: nl`,
`accept-language: nl-NL`.

## Auth

- Discovery: `/.well-known/openid-configuration`. Grant types include `authorization_code`, `refresh_token`,
  `passwordless_email`, `otp_email`, `otac`, token-exchange, device_code.
- Web client: `client_id=consumer_web_je` (public, no secret).
  - `scope=openid profile mobile_scope offline_access`
  - `acr_values=tenant:nl` (optionally `idp:...`, `partner:...`, `rqst-ctx:...`)
  - `response_type=code` + PKCE S256
  - Accepted `redirect_uri`: `https://www.thuisbezorgd.nl/signin-oidc` (query string allowed).
    Loopback (`http://127.0.0.1:*`, `http://localhost`) is **rejected**.
  - Device authorization: `unauthorized_client` for `consumer_web_je`.
- Refresh: `POST /connect/token` form `client_id=consumer_web_je&grant_type=refresh_token&refresh_token=...`.
  No captcha.
- BFF token for cw-api: `POST /connect/token` with `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`,
  `requested_token_type=urn:ietf:params:oauth:token-type:access_token_bff`,
  `subject_token=<access_token>`, `subject_token_type=urn:ietf:params:oauth:token-type:access_token`, `tenant=nl`.

### Hosted login (passwordless email code)

The authorize request redirects to `/account/login?ReturnUrl=/connect/authorize/callback?...` (React SPA).
SPA calls (axios, `withCredentials`, headers `accept-tenant`, `x-csrf: 1`, `X-Jet-Application: OneWeb`):

1. `POST /applications/authenticationservice/credentials/email/validate`
   `{"email", "returnUrl"}` + header `x-jet-captcha: <Cloudflare Turnstile token>` → sends code email.
2. `POST /applications/authenticationservice/credentials/otp/validate` `{"otp", "returnUrl"}`
   → `{validatedReturnUrl}`; browser then follows it → redirect to `redirect_uri?code=...`.

**Turnstile is enforced server-side** (`400 Turnstile challenge failed` without a valid token). Production
site keys: `je=0x4AAAAAAA-bzDjBshW13_-2`, `tkwy=0x4AAAAAAA-tMWDGhBOJ9-AV`.

Spike history: the first spikes loaded the login page from the generic `jet-external.com` host, where the
`tkwy` key fails with Turnstile error **110200 (domain not allowed)**. That, not automation detection,
is why no token appeared. On `auth.thuisbezorgd.nl`, a normal Chromium window started by jet-cli (no
DevTools protocol, just a helper extension) gets a token without any interaction. We still never try to
bypass the challenge: if it ever asks for a click, the user clicks.

`jet login` implementation notes:
- Chromium is launched with `--app=http://127.0.0.1:<port>/<nonce>/start` and `--load-extension`. The start
  page hands jet-cli's address to the extension, then redirects to the authorize URL.
- Extension files are static and live in a directory named after their content hash: Chromium caches an
  unpacked extension's service worker in the profile and ignores rewritten files.
- The submit button is a `pie-button` web component; `.click()` on it doesn't submit. `form.requestSubmit()`
  does. React-controlled inputs are filled via the native value setter plus `input` events.
- Login page paths: `/account/login` (email), `/account/mfa` (code, `input[name=otpCode]`).

Older v31 BFF (`cw-api.takeaway.com/api/v31/user/login`, username+password+2FA) used by
github.com/d-kold/bakasur — superseded.

## Ordering APIs (paths from the web bundle; `{t}` = tenant)

- Geocoding: `POST /geocode/{t}` `{"addressLines": ["Damrak 1, Amsterdam"], "noPostcodeFilter": false}` → GeoJSON
  Feature (`geometry.coordinates` = [lng, lat], `properties.structuredAddress`). Anonymous.
- Discovery: `GET /discovery/{t}/restaurants/enriched?latitude=&longitude=&serviceType=delivery|collection&ratingsOutOfFive=true`
  with `Accept: application/json;v=3`. Anonymous. Fees are in top-level `deliveryFees.restaurants[id]` (cents).
  (`bypostcode/{pc}` only resolves 4-digit postcodes.)
- Search: `/search/restaurants/{t}`, `/search/menu/{t}/{restaurant}`, `/search/autocomplete/{t}`
- Menu (static, anonymous, CDN): `{MENU_CDN}/{slug}_{t}_manifest.json` → `Menus[]` (one per service type, with
  `MenuGroupId`, `Categories[].ItemIds`), `ItemsUrl` (`Items[].Variations[]` with `BasePrice` in euros,
  `ModifierGroupsIds`, `MenuGroupIds`), `ItemDetailsUrl` (`ModifierGroups[]` → `ModifierSets[].Modifier`).
- Menu (live): `GET /restaurant/{t}/{slug}/menu/dynamic?latLong=&areaId=` → `OfflineVariationIds`,
  `OfflineModifierIds` (sold out). Anonymous.
- Product extras: `/restaurants/{t}/{id}/products/{pid}/customizations|details|information`
- Basket (works anonymously; send Bearer when logged in):
  - create `POST /basket` `{deals:[], products:[{date, productId: <variation id>, quantity, customerNotes,
    dealGroups:[], modifierGroups:[{modifierGroupId, modifiers:[{modifierId: <Modifier.Id>, quantity}]}]}],
    orderDetails:{location:{zipCode, geoLocation:{latitude, longitude}}}, menuGroupId, restaurantSeoName,
    serviceType, consents:[]}`
  - add `PUT /basket/{id}` `{basketId, deal:{added:[]}, product:{added:[…]}, orderDetails:{location:{zipCode:{value},
    geoLocation:{value:{…}}}}, selectedServiceType:{date, value}, consents:[], restaurantSeoName}`
  - quantity `PUT` `{basketId, product:{updated:[{date, basketProductId, quantity:{total}, modifierGroups, dealGroups:[]}]}}`
  - remove `PUT` `{basketId, product:{removed:[{date, basketProductId, modifierGroups, dealGroups:[]}]}}`
  - `GET /basket/{id}`. Response is PascalCase: `BasketSummary.Products[].BasketProductIds`, `BasketTotals`,
    `Adjustments` (service fee), `DeliveryCharge`, `Prompts.SpendMore`.
- Checkout (Bearer token; checkout id = basket id):
  - `GET /checkout/{t}/{basketId}` → `purchase` (groups/products, `lineItems`, `total.price.amount` in cents),
    `customer`, `fulfilment.time` / `.location`, `isFulfillable`, `issues[].code` (e.g. `FIRST_NAME_REQUIRED`,
    `PHONE_NUMBER_REQUIRED`, `FULFILMENT_TIME_REQUIRED`/`_UNAVAILABLE`, `ADDRESS_LINES_REQUIRED`,
    `LOCALITY_REQUIRED`, `POSTAL_CODE_REQUIRED`, `GEOLOCATION_REQUIRED`).
  - `PATCH` same URL, `Content-Type: application/json-patch+json`, ops `{op:"add", path, value}`:
    `/customer` `{firstName, lastName, phoneNumber}`, `/fulfilment/time` `{asap, scheduled: {from, to} | null}`,
    `/fulfilment/location` `{address: {lines, locality, administrativeArea, postalCode}, geolocation: {latitude, longitude}}`,
    `/notes/courier|kitchen|order` `{note}`, `/voucher` `{code}`, tipping, consents. Returns the checkout.
  - `GET …/fulfilment/availabletimes` → `times[{from, to}]` (UTC); ASAP gives `FULFILMENT_TIME_UNAVAILABLE`
    when the restaurant isn't open yet, then schedule a slot.
  - `GET …/options` → tipping, which notes are accepted, vouchers.
- Payment methods: `GET /payment/{t}/partners/{restaurantId}/options?serviceType=&checkoutId={basketId}` →
  `availablePaymentTypes[{paymentType, displayName, behaviour: external|native, status}]`, `savedPaymentTypes`.
  `external` (ideal, creditcard, paypal_rest) end on a payment page; `native` (applepay, googlepay) need an SDK.
- Pay: `POST /checkout/{t}/{basketId}/payments` with headers `x-jet-application-id: 16`,
  `x-jet-application-version`, body `{currency, total (cents), methods: [{type, amount, fee}], returnUrl}`.
  → 201 `{id, status: {code: "awaitingAction", redirect: {url (e.g. pay.ideal.nl), pollingPageUrl
  (pay.thuisbezorgd.nl/nl/status/…)}}}`; `accepted` (+orderId) for methods without a payment step; `rejected`
  reasons include `totalsDoNotMatch`, `duplicate`, `paymentRefused`. Until paid there is **no order**
  (`GET /consumer/me/orders/{t}/{id}` → 404); once paid it shows up in the order list with
  `basket.basketId` = the basket id. Tested 2026-10-17 with iDEAL (left unpaid).
- Orders: `/consumer/me/orders/{t}`, reorder `/consumer/me/orders/{t}/{id}/newbasket`
