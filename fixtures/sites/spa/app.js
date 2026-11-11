// A tiny single-page app: client-side routes, content fetched from /api,
// and images that only load once scrolled into view.
const routes = { "/": "/api/home.json", "/about": "/api/about.json" };

async function render(path) {
  const app = document.getElementById("app");
  const source = routes[path];
  if (!source) {
    app.innerHTML = "<h1>Not found</h1>";
    return;
  }
  const page = await (await fetch(source)).json();
  const images = page.images
    .map((src) => `<img class="lazy" data-src="${src}" alt="" width="200" height="120">`)
    .join("");
  app.innerHTML = `
    <nav><a href="/" data-link>Home</a> · <a href="/about" data-link>About</a></nav>
    <h1>${page.title}</h1>
    <p>${page.body}</p>
    <div class="spacer"></div>
    ${images}`;
  document.title = page.title;
  loadImagesWhenVisible();
}

function loadImagesWhenVisible() {
  const observer = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (entry.isIntersecting) {
        entry.target.src = entry.target.dataset.src;
        observer.unobserve(entry.target);
      }
    }
  });
  document.querySelectorAll("img.lazy").forEach((img) => observer.observe(img));
}

document.addEventListener("click", (event) => {
  const link = event.target.closest("a[data-link]");
  if (!link) return;
  event.preventDefault();
  history.pushState(null, "", link.getAttribute("href"));
  render(location.pathname);
});
window.addEventListener("popstate", () => render(location.pathname));
render(location.pathname);
