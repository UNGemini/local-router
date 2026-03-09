# Hosting the Wheels Router Nano Web Demo

The web demo is a single static HTML file with embedded CSS and JavaScript. It runs entirely in the browser using WASM, so you don't need a backend server.

## Quick Start (Local)

```bash
# Open in your default browser
open index.html

# Or serve locally with Python
python3 -m http.server 8000
# Then visit http://localhost:8000
```

## Deployment Options

### Option 1: GitHub Pages (Easiest, Free)

Push to GitHub and enable Pages:

```bash
# Create a gh-pages branch (if needed)
git checkout -b gh-pages

# Or just enable Pages on main branch in GitHub UI:
# Settings > Pages > Build and deployment > Source: Deploy from a branch
# Select main / root
```

Your demo will be live at: `https://YOUR-USERNAME.github.io/wheels-router-nano/`

### Option 2: Netlify (Free Tier, Recommended)

1. Push repo to GitHub
2. Connect to [Netlify](https://netlify.com)
3. Configure build (you don't need one - just set Publish directory to root)
4. Deploy

Your demo will be live at a Netlify subdomain (e.g., `wheels-router-nano-123.netlify.app`)

### Option 3: Vercel (Free Tier)

1. Push repo to GitHub
2. Import project at [Vercel](https://vercel.com)
3. Vercel auto-detects it's a static site
4. Deploy

Your demo will be live at a Vercel subdomain.

### Option 4: Self-Hosted (VPS/Server)

Run any static file server:

```bash
# Using Python
python3 -m http.server 8000

# Using Node.js http-server
npx http-server

# Using Ruby
ruby -run -ehttpd . -p 8000

# Using PHP
php -S localhost:8000
```

Then point your domain's DNS to your server.

### Option 5: Cloud Storage + CDN

Deploy to S3, Google Cloud Storage, or Azure Blob Storage + CloudFront/CDN for global distribution.

## Data Files

The demo loads transit data from `data/sf.wheelsrouter` (relative path).

### To use different data:

1. Build your `.wheelsrouter` file via the Python pipeline:
   ```bash
   python3 pipeline/build.py path/to/gtfs.zip path/to/osm.pbf data/my-city.wheelsrouter
   ```

2. Update `index.html` to load it:
   ```javascript
   // Line ~800 in index.html
   const dataResponse = await fetch('data/my-city.wheelsrouter');
   ```

3. Redeploy

## For Production

- Minimize `index.html` with a tool like `html-minifier`
- Compress WASM with Brotli (most CDNs do this automatically)
- Set cache headers on static files (1 year for WASM/JS, shorter for index.html)
- Enable GZIP compression on your server
- Consider serving from a CDN for faster global access

## Example Netlify Deploy (1 minute setup)

```bash
# 1. Install Netlify CLI
npm install -g netlify-cli

# 2. Deploy (will ask which site/account)
netlify deploy --prod

# Done! Check output for your live URL
```

## Updating the Demo

After rebuilding WASM with `wasm-pack build`:

1. Commit changes
2. Push to GitHub
3. Your hosting platform auto-redeploys (GitHub Pages, Netlify, Vercel)
4. Changes live in ~1-5 minutes

That's it. The entire system is serverless.
